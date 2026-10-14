//! The generation 1 store fixture: a bundle of one store holding one
//! object of every type, written once by a pinned build and read by
//! every later one. The write side of the format is pinned by the
//! layout snapshot tests in each type module; this test pins the read
//! side against data the current build did not write.
//!
//! The restore procedure it exercises is the one a backup needs: a
//! bare repository, a fetch of `refs/gage/*`, then `gage store init`
//! to stamp the version and install the hooks.
//!
//! Regenerate the bundle only when a new generation needs one:
//!
//! ```text
//! cargo test -p gage-store --test fixture write_generation_1_fixture -- --ignored
//! ```

use std::any::Any;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use gage_session::{
    ContentSink, ContentSource, Driver, DriverError, NativeSession, SessionAttrs, Source,
    SourceOptions, StoredSession,
};
use gage_store::{
    AttachmentSpec, AttachmentStore, DatasetOrigin, DatasetStore, IssueInput, IssueStatus,
    IssueStore, KeyStore, NoteInput, NoteStore, NoteValue, ScanStore, SessionStore, Store,
    TagStore, TaskStatus,
};

const BUNDLE: &str = "tests/fixtures/generation-1.bundle";
const SESSION_CONTENT: &str = "{\"type\":\"user\",\"message\":\"hello\"}\n";

fn bundle_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(BUNDLE)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

struct FakeSession {
    content: String,
}

impl SessionAttrs for FakeSession {
    fn native_mtime(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
    fn native_size(&self) -> u64 {
        self.content.len() as u64
    }
    fn is_empty(&self) -> bool {
        false
    }
    fn project_name(&self) -> Option<&str> {
        Some("fixture-project")
    }
    fn title(&self) -> Option<&str> {
        Some("Fixture session")
    }
    fn model(&self) -> Option<&str> {
        Some("fixture-model")
    }
    fn message_count(&self) -> Option<u64> {
        Some(1)
    }
    fn line_count(&self) -> Option<u64> {
        Some(1)
    }
}

impl NativeSession for FakeSession {
    fn id(&self) -> &str {
        "fixture-1"
    }
    fn session_type(&self) -> &str {
        "fake"
    }
    fn source(&self) -> &str {
        "fake:fixture-1"
    }
    fn attrs(&self) -> &dyn SessionAttrs {
        self
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct FakeDriver;

impl Driver for FakeDriver {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn version(&self) -> &'static str {
        "0.1"
    }
    fn schemes(&self) -> &'static [&'static str] {
        &["fake"]
    }
    fn open_source(&self, _options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
        Err(DriverError::Other("open_source not used".into()))
    }
    fn write_native(
        &self,
        session: &mut dyn NativeSession,
        sink: &mut dyn ContentSink,
    ) -> Result<String, DriverError> {
        let fake = session
            .as_any()
            .downcast_ref::<FakeSession>()
            .ok_or_else(|| DriverError::Other("not a FakeSession".into()))?;
        let mut w = sink.create("session.jsonl").map_err(DriverError::Io)?;
        w.write_all(fake.content.as_bytes())
            .map_err(DriverError::Io)?;
        Ok("fake-lines 1".to_string())
    }
    fn read_stored(
        &self,
        _native_id: String,
        _content_format: &str,
        _source: Box<dyn ContentSource>,
    ) -> Result<Box<dyn StoredSession>, DriverError> {
        Err(DriverError::Other("read_stored not used".into()))
    }
}

/// Write the fixture store and bundle it. Ignored: it runs by hand when
/// a generation first needs a fixture, never in the suite.
#[test]
#[ignore]
fn write_generation_1_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("store.git");
    gage_store::init(&path).unwrap();
    let store = Store::open(&path).unwrap();

    let note_id = NoteStore::from(&store)
        .create(NoteInput {
            name: "fixture-note",
            value: NoteValue::Text("hello".into()),
            author: "user:fixture",
            target: None,
            metadata: Some(serde_json::json!({"kind": "text"})),
            work_id: Some("fixture:run:1"),
        })
        .unwrap();
    NoteStore::from(&store)
        .create(NoteInput {
            name: "fixture-rating",
            value: NoteValue::Json(serde_json::json!({"score": 3})),
            author: "user:fixture",
            target: Some(&format!("note:{note_id}")),
            metadata: None,
            work_id: None,
        })
        .unwrap();

    let session = SessionStore::from(&store)
        .add(
            &FakeDriver,
            &mut FakeSession {
                content: SESSION_CONTENT.into(),
            },
        )
        .unwrap();

    let datasets = DatasetStore::from(&store);
    let dataset_id = datasets
        .create_with_origin(&DatasetOrigin::Scan("FIXTURESCAN1".into()))
        .unwrap();
    datasets
        .sessions_link(&dataset_id, std::slice::from_ref(&session.id))
        .unwrap();
    let dataset_sha = datasets.get(&dataset_id).unwrap().commit_sha;

    let root = tmp.path().join("attach");
    write(&root.join("CLAUDE.md"), "rules\n");
    write(&root.join("settings.json"), "{\"a\":1}\n");
    let target = format!("session:{}", session.id);
    AttachmentStore::from(&store)
        .add(&AttachmentSpec {
            name: Some("fixture-attachment"),
            key: None,
            targets: std::slice::from_ref(&target),
            root: &root,
            includes: &["*".to_string()],
            excludes: &[],
        })
        .unwrap();

    let issues = IssueStore::from(&store);
    let issue_id = issues
        .create(IssueInput {
            name: "fixture-issue",
            title: "Fixture issue",
            description: Some("A description.\n"),
            author: "user:fixture",
            status: IssueStatus::Pending,
            evidence: std::slice::from_ref(&note_id),
            key: Some("fixture/issue"),
        })
        .unwrap();
    issues
        .comment(&issue_id, "user:fixture", "A comment.")
        .unwrap();
    issues
        .set_status(&issue_id, IssueStatus::Open, None, "user:fixture", None)
        .unwrap();

    let scan_dir = tmp.path().join("scan");
    write(
        &scan_dir.join("attrs.json"),
        r#"{"runtime":"gage fixture","started":1000,"stopped":2000,"canceled":false,"tasks":{"total":1,"completed":1,"failed":0}}"#,
    );
    write(
        &scan_dir.join("tasks/fixture/run/attrs.json"),
        r#"{"status":"completed","started":1000,"stopped":2000,"worked_ms":1000}"#,
    );
    write(&scan_dir.join("logs/out"), "fixture\n");
    write(&scan_dir.join("dataset.link"), &format!("{dataset_sha}\n"));
    let note_sha = git(
        &path,
        &["rev-parse", &format!("refs/gage/1/object/{note_id}")],
    );
    write(&scan_dir.join("notes.link"), &note_sha);
    write(
        &scan_dir.join(format!("watermarks/{}/fixture:run:1", session.id)),
        &format!("{} 1\n", session.commit_sha),
    );
    ScanStore::from(&store)
        .create("FIXTURESCAN1", &scan_dir)
        .unwrap();

    TagStore::from(&store)
        .add("fixture-tag", &note_id, false)
        .unwrap();
    drop(store);

    let bundle = bundle_path();
    fs::create_dir_all(bundle.parent().unwrap()).unwrap();
    git(
        &path,
        &[
            "bundle",
            "create",
            bundle.to_str().unwrap(),
            "--glob=refs/gage/",
        ],
    );
}

/// Restore the fixture into a fresh repository and read every object.
#[test]
fn reads_the_generation_1_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("store.git");
    fs::create_dir_all(&path).unwrap();
    git(&path, &["init", "--bare", "--quiet"]);
    git(
        &path,
        &[
            "fetch",
            "--quiet",
            bundle_path().to_str().unwrap(),
            "+refs/gage/*:refs/gage/*",
        ],
    );
    gage_store::init(&path).unwrap();
    let store = Store::open(&path).unwrap();
    store.fsck().unwrap();

    let notes = NoteStore::from(&store);
    let tagged = store.resolve_in("fixture-tag", None).unwrap();
    let note = notes.get(&tagged.id).unwrap();
    assert_eq!(note.name, "fixture-note");
    assert_eq!(note.value, NoteValue::Text("hello".into()));
    assert_eq!(note.metadata, Some(serde_json::json!({"kind": "text"})));
    assert_eq!(note.work_id.as_deref(), Some("fixture:run:1"));
    let listed: Vec<_> = notes.iter().unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(listed.len(), 2);
    let rating = listed.iter().find(|n| n.name == "fixture-rating").unwrap();
    assert_eq!(
        rating.value,
        NoteValue::Json(serde_json::json!({"score": 3}))
    );
    assert_eq!(
        rating.target.as_deref(),
        Some(format!("note:{}", note.id).as_str())
    );

    let sessions = SessionStore::from(&store);
    let session_ids: Vec<_> = sessions
        .query()
        .iter()
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(session_ids.len(), 1);
    let session = &session_ids[0];
    assert_eq!(session.attrs.native_id, "fixture-1");
    assert_eq!(session.attrs.content_format, "fake-lines 1");
    assert_eq!(session.attrs.driver, "fake 0.1");
    assert_eq!(
        session.attrs.summary.title.as_deref(),
        Some("Fixture session")
    );
    let content = sessions.content(&session.commit_sha);
    assert_eq!(content.paths().unwrap(), ["session.jsonl"]);
    let mut bytes = String::new();
    content
        .open("session.jsonl")
        .unwrap()
        .read_to_string(&mut bytes)
        .unwrap();
    assert_eq!(bytes, SESSION_CONTENT);

    let datasets = DatasetStore::from(&store);
    let dataset_list: Vec<_> = datasets.iter().unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(dataset_list.len(), 1);
    let dataset = &dataset_list[0];
    assert_eq!(
        dataset.origin,
        Some(DatasetOrigin::Scan("FIXTURESCAN1".into()))
    );
    let members = datasets.sessions(&dataset.id).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].id, session.id);

    let attachments = AttachmentStore::from(&store);
    let attachment_list: Vec<_> = attachments.iter().unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(attachment_list.len(), 1);
    let attachment = &attachment_list[0];
    assert_eq!(attachment.attrs.name.as_deref(), Some("fixture-attachment"));
    assert_eq!(
        attachment.attrs.targets,
        [format!("session:{}", session.id)]
    );
    let files = attachments.files(&attachment.commit_sha).unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, ["CLAUDE.md", "settings.json"]);
    assert_eq!(
        attachments
            .read_file(&attachment.commit_sha, "CLAUDE.md")
            .unwrap(),
        Some(b"rules\n".to_vec())
    );

    let issues = IssueStore::from(&store);
    let issue = issues.live_under_key("fixture/issue").unwrap().unwrap();
    assert_eq!(issue.title, "Fixture issue");
    assert_eq!(issue.status, IssueStatus::Open);
    assert_eq!(issue.description.as_deref(), Some("A description.\n"));
    assert_eq!(issue.evidence.len(), 1);
    assert_eq!(store.read_header(&issue.evidence[0]).unwrap().id, note.id);
    let events: Vec<&str> = issue.changes.iter().map(|c| c.event.as_str()).collect();
    assert_eq!(events, ["create", "comment", "status"]);
    assert_eq!(issues.iter().unwrap().count(), 1);
    // The issue's key and the named attachment's default key
    let keys = KeyStore::from(&store).list().unwrap();
    assert_eq!(keys.len(), 2);
    let issue_key = keys.iter().find(|k| k.type_name == "issue").unwrap();
    assert_eq!(issue_key.key, "fixture/issue");
    assert_eq!(issue_key.id, issue.id);
    assert_eq!(
        keys.iter().filter(|k| k.type_name == "attachment").count(),
        1
    );

    let scans = ScanStore::from(&store);
    let scan = scans.get("FIXTURESCAN1").unwrap();
    assert_eq!(scan.content.attrs.runtime, "gage fixture");
    assert_eq!(scan.content.tasks.len(), 1);
    assert_eq!(scan.content.tasks[0].attrs.status, TaskStatus::Completed);
    assert_eq!(scan.content.notes.len(), 1);
    assert_eq!(
        store.read_header(&scan.content.notes[0]).unwrap().id,
        note.id
    );
    assert_eq!(
        scan.content.dataset.as_deref(),
        Some(dataset.commit_sha.as_str())
    );
    assert_eq!(scan.content.watermarks.len(), 1);
    assert_eq!(scan.content.watermarks[0].oid, session.id);
    assert_eq!(scan.content.watermarks[0].work_id, "fixture:run:1");
    assert_eq!(scan.content.watermarks[0].version, session.commit_sha);
    assert_eq!(scan.content.watermarks[0].mark, 1);
    assert_eq!(scan.content.logs, ["out"]);
}
