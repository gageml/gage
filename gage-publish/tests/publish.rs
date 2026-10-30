//! The publish engine over a Claude session added to a temporary
//! store: filters at init, the definition edited by hand and
//! refreshed, and the staged content and `filtered.json` they
//! produce.

use std::fs;

use gage_publish::{
    Allow, FilterSpec, InitArgs, PublishDefinition, PublishError, Publisher, Refreshed, candidate,
    format_lines,
};
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

/// Stage `filters` over the fixture's original with no segment.
fn stage(publisher: &Publisher<'_>, original: &SessionRecord, filters: &[FilterSpec]) -> String {
    publisher
        .init(original, filters, &[], "session:test", &InitArgs::default())
        .unwrap()
        .id
}

fn write_definition(publisher: &Publisher<'_>, id: &str, definition: &PublishDefinition) {
    fs::write(
        publisher.store().definition_file(id),
        definition.to_jsonc_text(None),
    )
    .unwrap();
}

/// Edit the definition of `id` the way a hand edit does: read the
/// file, change it, write it back, and refresh as `user:test`.
fn edit(
    publisher: &Publisher<'_>,
    id: &str,
    change: impl FnOnce(&mut PublishDefinition),
) -> Refreshed {
    let mut definition = publisher.definition(id).unwrap();
    change(&mut definition);
    write_definition(publisher, id, &definition);
    publisher.refresh(id, "user:test").unwrap()
}

/// The instance count of the redaction whose literal is `literal`.
fn instances_of(publisher: &Publisher<'_>, id: &str, literal: &str) -> usize {
    let definition = publisher.definition(id).unwrap();
    let counts = publisher.instances(id).unwrap();
    let index = definition
        .redact
        .iter()
        .position(|t| t.literal == literal)
        .unwrap_or_else(|| panic!("no redaction {literal:?}: {definition:#?}"));
    *counts.get(&index).unwrap()
}

#[test]
fn init_runs_the_named_filters_and_records_their_instances() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let filters = [
        "pii".parse::<FilterSpec>().unwrap(),
        "secret".parse::<FilterSpec>().unwrap(),
    ];
    let id = stage(&publisher, &f.original, &filters);

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
    let counts = publisher.instances(&id).unwrap();
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

    let records = filtered(&f.publish_root, &id);
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

/// The file `init` writes names the session with its title, records
/// the init arguments, and reads back; a refresh rewrites it with the
/// title kept.
#[test]
fn init_writes_the_annotated_file_with_the_title_and_init_args() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = InitArgs {
        audience: Some("public".into()),
        uses: Some("reading".into()),
        notes: Some("For the blog".into()),
    };
    let id = publisher
        .init(&f.original, &[], &[], "session:test", &init)
        .unwrap()
        .id;
    let path = publisher.store().definition_file(&id);
    assert!(path.extension().is_some_and(|e| e == "jsonc"), "{path:?}");
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("// A pending publish."), "{text}");
    assert!(
        text.contains(&format!("\"session\": \"{}\", // title: ", f.original.id)),
        "{text}"
    );
    assert!(
        text.contains(r#""init": {"audience":"public","use":"reading","notes":"For the blog"}"#),
        "{text}"
    );
    let definition = publisher.definition(&id).unwrap();
    assert_eq!(definition.init, init);
    assert!(definition.filters.is_empty() && definition.redact.is_empty());

    publisher.refresh(&id, "user:test").unwrap();
    let again = fs::read_to_string(&path).unwrap();
    assert_eq!(
        again, text,
        "a refresh without a change rewrites the same file"
    );
}

/// A redaction row added by hand takes effect everywhere its text
/// appears, a row without an author takes the refresh's, a row that
/// matches nothing is reported by position and kept, and a changed
/// reason or mark takes effect.
#[test]
fn refresh_applies_redact_rows_and_reports_an_unmatched_row() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let id = stage(&publisher, &f.original, &[]);
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 5);

    let refreshed = edit(&publisher, &id, |d| {
        d.push_redact("garrett", "pii", "user:hand", true);
        d.push_redact("fucking", "language", "", false);
        d.push_redact("nothing-like-this", "secret", "user:hand", true);
    });
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
    assert_eq!(instances_of(&publisher, &id, "garrett"), 3);
    assert_eq!(instances_of(&publisher, &id, "fucking"), 1);
    assert_eq!(instances_of(&publisher, &id, "nothing-like-this"), 0);

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

    let records = filtered(&f.publish_root, &id);
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

    // A changed reason and mark take effect; a changed literal is a
    // new match
    edit(&publisher, &id, |d| {
        d.redact.truncate(1);
        d.redact[0].reason = "privacy".into();
        d.redact[0].mark = false;
    });
    let line = staged_lines(&f.publish_root, &id).remove(0);
    assert!(line.contains("the fucking hell"), "{line}");
    assert!(
        line.contains(r#""cwd":"/home//proj""#),
        "removed without a mark: {line}"
    );
    let records = filtered(&f.publish_root, &id);
    assert!(
        records.iter().all(|r| r.reason == "privacy"),
        "{records:#?}"
    );
}

/// A filter is a name, and `custom` takes its prompt as its whole
/// instruction: `init` refuses it without one, and so does a refresh.
#[test]
fn custom_requires_a_prompt_and_any_filter_takes_one() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let bare = FilterSpec {
        name: "custom".into(),
        prompt: None,
    };
    let err = publisher
        .init(
            &f.original,
            &[bare],
            &[],
            "session:test",
            &InitArgs::default(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("requires a prompt"), "{err}");
    assert!(publisher.store().list().unwrap().is_empty());

    let custom = FilterSpec::new("custom", Some("Remove every duck.".into())).unwrap();
    let pii = FilterSpec::new("pii", Some("Watch for street addresses.".into())).unwrap();
    let id = stage(&publisher, &f.original, &[custom, pii]);
    let definition = publisher.definition(&id).unwrap();
    assert_eq!(definition.filters[0].name, "custom");
    assert_eq!(
        definition.filters[0].prompt.as_deref(),
        Some("Remove every duck.")
    );
    assert_eq!(
        definition.filters[1].prompt.as_deref(),
        Some("Watch for street addresses.")
    );
    let text = fs::read_to_string(publisher.store().definition_file(&id)).unwrap();
    assert!(
        text.contains(r#"    ["custom", "Remove every duck."],"#),
        "{text}"
    );
    assert!(
        text.contains(r#"    ["pii", "Watch for street addresses."],"#),
        "{text}"
    );

    let mut definition = publisher.definition(&id).unwrap();
    definition.filters[0].prompt = None;
    write_definition(&publisher, &id, &definition);
    let err = publisher.refresh(&id, "user:test").unwrap_err();
    assert!(err.to_string().contains("requires a prompt"), "{err}");
    definition.filters[0].name = "piii".into();
    definition.filters[0].prompt = Some("x".into());
    write_definition(&publisher, &id, &definition);
    let err = publisher.refresh(&id, "user:test").unwrap_err();
    assert!(err.to_string().contains("piii"), "{err}");
    assert_eq!(
        publisher.definition(&id).unwrap().filters[0].name,
        "piii",
        "an invalid file is left as written"
    );
}

/// The `harness-records` filter leaves out the entries the harness
/// kept about the run: the API request on line 5 goes, with one line
/// record under the filter, and the descriptive fields of the entries
/// that stay go with it.
#[test]
fn harness_records_filter_leaves_out_run_records_and_descriptive_fields() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let id = stage(
        &publisher,
        &f.original,
        &["harness-records".parse::<FilterSpec>().unwrap()],
    );
    let lines = staged_lines(&f.publish_root, &id);
    assert_eq!(lines.len(), 4, "{lines:#?}");
    assert!(
        !lines.iter().any(|l| l.contains("api-request")),
        "{lines:#?}"
    );
    let records = filtered(&f.publish_root, &id);
    let dropped: Vec<(u32, Option<u32>, &str, &str)> = records
        .iter()
        .filter(|r| r.kind == FilteredKind::Line)
        .map(|r| (r.line, r.line_end, r.reason.as_str(), r.author.as_str()))
        .collect();
    assert_eq!(
        dropped,
        [(5, Some(5), "harness-records", "filter:harness-records")]
    );
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
        .filter(|r| r.kind == FilteredKind::Value)
        .map(|r| r.reference.as_deref().unwrap())
        .collect();
    assert!(values.contains(&"$.message.usage"), "{values:?}");
    assert!(
        records
            .iter()
            .filter(|r| r.kind == FilteredKind::Value)
            .all(|r| r.reason == "harness-records" && r.author == "filter:harness-records")
    );
    let summary = staged_summary(&f.publish_root, &id);
    assert_eq!(summary["line_count"], 4, "{summary}");
    publisher.check(&id).unwrap();

    // A literal only on a dropped line matches nothing
    let refreshed = edit(&publisher, &id, |d| {
        d.push_redact("home_dir", "system", "user:test", true);
    });
    assert_eq!(refreshed.unmatched, [(1, "home_dir".to_string())]);
}

/// The `harness-context` filter leaves out exactly what the harness put
/// in the model's context: the attachment, the meta user entry, and the
/// request shape and blob go as one run of lines, while the API request
/// record stays and the token usage stays.
#[test]
fn harness_context_filter_leaves_out_context_entries_and_nothing_else() {
    let f = fixture_with(CONTEXT_SESSION);
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let id = stage(
        &publisher,
        &f.original,
        &["harness-context".parse::<FilterSpec>().unwrap()],
    );
    let lines = staged_lines(&f.publish_root, &id);
    assert_eq!(lines.len(), 3, "{lines:#?}");
    assert!(lines[0].contains("\"content\":\"Hello\""), "{}", lines[0]);
    assert!(lines[1].contains("\"usage\""), "{}", lines[1]);
    assert!(lines[2].contains("\"api-request\""), "{}", lines[2]);
    let records = filtered(&f.publish_root, &id);
    let dropped: Vec<(u32, Option<u32>, &str, &str)> = records
        .iter()
        .filter(|r| r.kind == FilteredKind::Line)
        .map(|r| (r.line, r.line_end, r.reason.as_str(), r.author.as_str()))
        .collect();
    assert_eq!(
        dropped,
        [(2, Some(5), "harness-context", "filter:harness-context")]
    );
    assert!(
        records.iter().all(|r| r.kind == FilteredKind::Line),
        "{records:#?}"
    );
    publisher.check(&id).unwrap();
}

/// `init` rejects a filter given twice.
#[test]
fn init_rejects_a_duplicate_filter() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let pii = "pii".parse::<FilterSpec>().unwrap();
    let dup = publisher
        .init(
            &f.original,
            &[pii.clone(), pii],
            &[],
            "session:test",
            &InitArgs::default(),
        )
        .unwrap_err();
    assert!(matches!(dup, PublishError::Input(_)), "{dup}");
    assert!(publisher.store().list().unwrap().is_empty());
}

/// A filter added to the file runs on refresh and keeps everything
/// else; a filter removed from the file takes its redactions with it
/// and leaves the user's; a filter listed twice is rejected.
#[test]
fn refresh_runs_an_added_filter_and_drops_a_removed_filters_rows() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let id = stage(
        &publisher,
        &f.original,
        &["secret".parse::<FilterSpec>().unwrap()],
    );
    edit(&publisher, &id, |d| {
        d.push_redact("fucking", "language", "user:test", true);
    });

    edit(&publisher, &id, |d| {
        d.filters.push(gage_publish::FilterEntry::new(
            &"pii".parse::<FilterSpec>().unwrap(),
        ));
    });
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
    let err = publisher.refresh(&id, "user:test").unwrap_err();
    assert!(err.to_string().contains("more than once"), "{err}");

    edit(&publisher, &id, |d| {
        d.filters.retain(|f| f.name != "secret");
    });
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
#[test]
fn refresh_reruns_filters_and_keeps_rows_in_place() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let filters = [
        "pii".parse::<FilterSpec>().unwrap(),
        "secret".parse::<FilterSpec>().unwrap(),
    ];
    let id = stage(&publisher, &f.original, &filters);
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
    });
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
    });
    let back = publisher.definition(&id).unwrap();
    assert_eq!(back.redact.len(), 4, "{back:#?}");
    assert_eq!(back.redact[3].literal, key, "the filter's row came back");
    assert!(staged_lines(&f.publish_root, &id)[1].contains("Use key [REDACTED]"));
}

/// The title stays the original's: the driver takes it from the first
/// prompt, and redacting that prompt does not retitle the publication.
#[test]
fn the_staged_summary_keeps_the_original_title() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let id = stage(&publisher, &f.original, &[]);

    let before = staged_summary(&f.publish_root, &id);
    let title = before["title"].clone();
    assert!(
        title.as_str().is_some_and(|t| t.contains("Hello")),
        "{before}"
    );

    edit(&publisher, &id, |d| {
        d.push_redact("Hello", "privacy", "user:test", true);
    });
    let after = staged_summary(&f.publish_root, &id);
    assert_eq!(after["title"], title, "{after}");
    assert_eq!(after["line_count"], 5, "{after}");
    assert!(
        staged_lines(&f.publish_root, &id)[0].contains("[REDACTED], what"),
        "the prompt itself is redacted"
    );
}

/// A user's row deleted by hand is gone; a filter's row deleted by
/// hand comes back unless its literal is on the allow list, where a
/// refresh leaves it out.
#[test]
fn an_allowed_literal_is_not_recorded_again() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let id = stage(
        &publisher,
        &f.original,
        &["secret".parse::<FilterSpec>().unwrap()],
    );
    edit(&publisher, &id, |d| {
        d.push_redact("garrett", "pii", "user:test", true);
    });
    let cwd = |publish_root: &std::path::Path| {
        let line = staged_lines(publish_root, &id).remove(0);
        line.contains("/home/[REDACTED]/proj")
    };
    assert!(cwd(&f.publish_root));

    edit(&publisher, &id, |d| {
        d.redact.retain(|t| t.literal != "garrett");
    });
    assert!(!cwd(&f.publish_root), "the username is back");
    let definition = publisher.definition(&id).unwrap();
    assert_eq!(definition.redact.len(), 1, "{definition:#?}");
    let key = definition.redact[0].literal.clone();
    assert_eq!(definition.redact[0].author, "filter:secret");

    edit(&publisher, &id, |d| {
        d.redact.clear();
    });
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
    });
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
#[test]
fn refresh_reapplies_a_hand_edited_file() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let id = stage(&publisher, &f.original, &[]);
    let path = publisher.store().definition_file(&id);
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
    let refreshed = publisher.refresh(&id, "session:hand").unwrap();
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
    let refreshed = publisher.refresh(&id, "session:hand").unwrap();
    assert_eq!(
        refreshed.instances, 0,
        "the username is only on dropped lines"
    );
    assert_eq!(refreshed.unmatched, [(1, "garrett".to_string())]);
    let lines = staged_lines(&f.publish_root, &id);
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(lines[0].contains(r#""uuid":"a1""#), "{}", lines[0]);
    assert!(lines[1].contains(r#""uuid":"a2""#), "{}", lines[1]);
    let records = filtered(&f.publish_root, &id);
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
    let after = staged_summary(&f.publish_root, &id);
    assert_eq!(after["line_count"], 2, "{after}");
    assert_eq!(after["output_tokens"], 10, "{after}");
    assert!(
        after["message_count"].as_u64().unwrap() < before["message_count"].as_u64().unwrap(),
        "{before} -> {after}"
    );
    assert_eq!(after["title"], before["title"], "{after}");
    publisher.check(&id).unwrap();

    fs::write(
        &path,
        file("  \"redact\": [[\"x\", \"bogus\", \"user:hand\", true]],"),
    )
    .unwrap();
    let err = publisher.refresh(&id, "session:hand").unwrap_err();
    assert!(err.to_string().contains("redaction 1"), "{err}");
    assert!(err.to_string().contains("bogus"), "{err}");
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 2);

    fs::write(&path, file("  \"segments\": [[\"9-2\", \"d\", \"u\"]],")).unwrap();
    let err = publisher.refresh(&id, "session:hand").unwrap_err();
    assert!(err.to_string().contains("9-2"), "{err}");

    fs::write(
        &path,
        "{\n  \"session\": \"other\",\n  \"filters\": [\"pii\"],\n}\n",
    )
    .unwrap();
    let err = publisher.refresh(&id, "session:hand").unwrap_err();
    assert!(err.to_string().contains("other"), "{err}");
    let err = publisher.apply(&id, true).unwrap_err();
    assert!(matches!(err, PublishError::Refused(_)), "{err}");
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 2);
}

/// A segment without lines drops nothing; once it has lines, every
/// line outside it is dropped; a changed or removed segment rebuilds.
#[test]
fn segments_drop_the_lines_outside_them() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let id = stage(&publisher, &f.original, &[]);

    edit(&publisher, &id, |d| {
        d.push_segment("The reply", &[], "session:s");
    });
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 5);

    edit(&publisher, &id, |d| d.segments[0].lines = vec![(2, 3)]);
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 2);
    let records = filtered(&f.publish_root, &id);
    assert_eq!(
        records
            .iter()
            .filter(|r| r.kind == FilteredKind::Line)
            .count(),
        2,
        "{records:#?}"
    );

    edit(&publisher, &id, |d| {
        d.segments[0].description = "The whole exchange".into();
        d.segments[0].lines = vec![(1, 5)];
    });
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 5);
    assert_eq!(
        publisher.definition(&id).unwrap().segments[0].description,
        "The whole exchange"
    );

    edit(&publisher, &id, |d| d.segments.clear());
    assert!(publisher.definition(&id).unwrap().segments.is_empty());
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 5);
}

/// The candidates are every text value of the kept lines that no
/// filter leaves out, in document order, each with the spans redacted
/// in it merged.
#[test]
fn candidates_are_the_disclosed_text_values_with_their_spans() {
    let f = fixture_with(SYSTEM_SESSION);
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let id = stage(
        &publisher,
        &f.original,
        &["system".parse::<FilterSpec>().unwrap()],
    );
    edit(&publisher, &id, |d| {
        d.push_redact("/home/garrett", "system", "user:test", true);
        d.push_segment("The lookup", &[(2, 4)], "user:test");
    });

    let candidates = publisher.candidates(&id).unwrap();
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
#[test]
fn apply_commits_the_derived_session_and_clears_the_staging() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let filters = [
        "pii".parse::<FilterSpec>().unwrap(),
        "secret".parse::<FilterSpec>().unwrap(),
    ];
    let init = publisher
        .init(
            &f.original,
            &filters,
            &[],
            "session:test",
            &InitArgs::default(),
        )
        .unwrap();
    edit(&publisher, &init.id, |d| {
        d.push_redact("fucking", "language", "user:test", false);
    });

    let commit = publisher.apply(&init.id, false).unwrap();

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
    let filters = [FilterSpec::new("custom", Some("Remove nothing".into())).unwrap()];
    let id = stage(&publisher, &f.original, &filters);
    let path = f
        .publish_root
        .join(&id)
        .join("files.d")
        .join("session.jsonl");
    let staged = fs::read_to_string(&path).unwrap();

    let refused = |edited: String, expect: &str| {
        fs::write(&path, edited).unwrap();
        let err = publisher.apply(&id, true).unwrap_err();
        assert!(matches!(err, PublishError::Refused(_)), "{err}");
        assert!(err.to_string().contains(expect), "{err}");
        assert_eq!(SessionStore::from(&f.store).iter().unwrap().count(), 1);
        assert!(f.publish_root.join(&id).is_dir());
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
    publisher.check(&id).unwrap();
}

/// The candidate text is each distinct disclosed value once, in order
/// of first occurrence, divided into splits that `--split-files`
/// writes one per file.
#[test]
fn candidate_texts_are_each_distinct_value_once_in_splits() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let id = stage(&publisher, &f.original, &[]);
    let candidates = publisher.candidates(&id).unwrap();
    let texts = publisher.candidate_texts(&id).unwrap();
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
#[test]
fn init_adds_segments_by_description() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let segments = ["The question".to_string(), "The reply".to_string()];
    let id = publisher
        .init(
            &f.original,
            &[],
            &segments,
            "session:s",
            &InitArgs::default(),
        )
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
    let text = fs::read_to_string(publisher.store().definition_file(&id)).unwrap();
    assert!(
        text.contains(r#"    [null, "The question", "session:s"],"#),
        "{text}"
    );
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 5);
    assert_eq!(publisher.changes(&id).unwrap(), 0);
}

/// `apply` refuses a definition with no filter and no segment, forced
/// or not: an unmodified session is pushed directly. A definition
/// whose filters and segments changed nothing is refused unless
/// forced, and a forced apply publishes the session as it is.
#[test]
fn apply_requires_a_filter_or_segment_and_a_change_unless_forced() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let sessions = SessionStore::from(&f.store);

    let empty = stage(&publisher, &f.original, &[]);
    for force in [false, true] {
        let err = publisher.apply(&empty, force).unwrap_err();
        assert!(matches!(err, PublishError::Refused(_)), "{err}");
        assert!(err.to_string().contains("no filter or segment"), "{err}");
    }
    assert!(f.publish_root.join(&empty).is_dir());
    assert_eq!(sessions.iter().unwrap().count(), 1);

    let filters = [FilterSpec::new("custom", Some("Remove nothing".into())).unwrap()];
    let unchanged = stage(&publisher, &f.original, &filters);
    assert_eq!(publisher.changes(&unchanged).unwrap(), 0);
    let err = publisher.apply(&unchanged, false).unwrap_err();
    assert!(matches!(err, PublishError::Unchanged), "{err}");
    assert!(f.publish_root.join(&unchanged).is_dir());
    assert_eq!(sessions.iter().unwrap().count(), 1);

    let commit = publisher.apply(&unchanged, true).unwrap();
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
    let id = stage(&publisher, &f.original, &filters);

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

    let records = filtered(&f.publish_root, &id);
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

    let attrs = staged_attrs(&f.publish_root, &id);
    assert!(attrs.get("project").is_none(), "{attrs}");

    // A literal found only inside a left-out value changes nothing,
    // and is reported as such
    let refreshed = edit(&publisher, &id, |d| {
        d.push_redact("git.corp.example", "system", "user:test", true);
    });
    assert_eq!(refreshed.unmatched, [(4, "git.corp.example".to_string())]);

    edit(&publisher, &id, |d| {
        d.filters.retain(|f| f.name != "system");
        d.redact.retain(|t| t.literal != "git.corp.example");
    });
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
    let records = filtered(&f.publish_root, &id);
    assert!(
        !records
            .iter()
            .any(|r| r.kind == FilteredKind::Attr || r.kind == FilteredKind::Value),
        "{records:#?}"
    );
}

/// The name on the reminder's git user line is a pii pattern literal:
/// found in the attachment, redacted there and in a quoted copy.
#[test]
fn the_git_user_name_is_redacted_from_the_reminder_and_its_quotes() {
    let session = concat!(
        r#"{"type":"attachment","uuid":"x1","timestamp":"2025-01-01T00:00:00Z","attachment":{"type":"session_context","context":{"gitStatus":"Current branch: main\n\nGit user: Alice Carol\n\nStatus:\n(clean)"}}}"#,
        "\n",
        r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:01Z","message":{"role":"user","content":"Hello"}}"#,
        "\n",
        r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:02Z","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"Commits are by Alice Carol."}],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        "\n",
    );
    let f = fixture_with(session);
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let id = stage(
        &publisher,
        &f.original,
        &["pii".parse::<FilterSpec>().unwrap()],
    );
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

#[test]
fn a_literal_inside_a_tool_input_is_a_span() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let id = stage(&publisher, &f.original, &[]);
    let key = "sk-ant-api03-PtYgjmUhBel31iEl2hpChYgCfrL1spNxnyVmihA-2O76UMFxFkM-R5Kjp1vRt_1fjORS-6ilI8ihN5KXSc7Tvo-hBKqFYAA";
    let command = "/message/content/0/input/command";

    edit(&publisher, &id, |d| {
        d.push_redact(key, "secret", "user:test", true);
        d.push_redact("api03", "secret", "user:test", true);
    });
    // The key: a span on line 2 and one in the command on line 3. The
    // substring: a span inside each of them
    assert_eq!(instances_of(&publisher, &id, key), 2);
    assert_eq!(instances_of(&publisher, &id, "api03"), 2);

    let records = filtered(&f.publish_root, &id);
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
    });
    assert_eq!(instances_of(&publisher, &id, "echo"), 1);
    let lines = staged_lines(&f.publish_root, &id);
    assert!(
        lines[2].contains(r#""command":"[REDACTED] [REDACTED]""#),
        "{}",
        lines[2]
    );
}
