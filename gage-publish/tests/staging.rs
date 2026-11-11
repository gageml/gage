//! The staging of a pending publish: the directory `init` writes, the
//! listing and lookup over it, and what `apply` and `abort` leave in
//! the store and on disk.

use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gage_publish::staging::{DEFINITION_ATTACHMENT, PublishDirLayout, Staging};
use gage_publish::{PublishDefinition, PublishError};
use gage_registry::driver::DriverRegistry;
use gage_session::SourceOptions;
use gage_store::{
    AttachmentStore, SessionAttrsRecord, SessionRecord, SessionStore, Store, StoreError,
};

const SESSION: &str = concat!(
    r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/w/proj","message":{"role":"user","content":"hello"}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"world"}],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
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
    let project_dir = claude.path().join("projects").join("-w-proj");
    fs::create_dir_all(&project_dir).unwrap();
    fs::write(project_dir.join(format!("{native_id}.jsonl")), SESSION).unwrap();

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

fn read_attrs(layout: &PublishDirLayout) -> SessionAttrsRecord {
    serde_json::from_slice(&fs::read(layout.session().attrs_file()).unwrap()).unwrap()
}

#[test]
fn init_stages_a_derived_session_directory() {
    let f = fixture();
    let staging = Staging::with_root(&f.store, &f.publish_root);

    let init = staging.init(&f.original).unwrap();

    let layout = staging.layout(&init.id);
    assert_eq!(layout.root(), f.publish_root.join(&init.id));
    assert_eq!(layout.id(), Some(init.id.as_str()));
    assert_eq!(
        layout.definition_file(),
        f.publish_root.join(&init.id).join("publish.jsonc")
    );
    assert_eq!(
        init.origin,
        format!("session:{}@{}", f.original.id, f.original.commit_sha)
    );

    let attrs = read_attrs(&layout);
    assert_eq!(attrs.origin, init.origin);
    assert_eq!(attrs.driver, f.original.attrs.driver);
    assert_eq!(attrs.content_format, f.original.attrs.content_format);
    assert_eq!(attrs.size, SESSION.len() as u64);
    assert_eq!(attrs.native_mtime, 0);
    assert_eq!(attrs.native_size, 0);

    let session = layout.session();
    assert_eq!(fs::read_to_string(session.changes_file()).unwrap(), "[]\n");
    let staged = fs::read_to_string(session.files_dir().join("session.jsonl")).unwrap();
    assert_eq!(staged, SESSION);
}

/// The listing holds every id-named directory with a staged
/// `attrs.json`, in id order, and passes over anything else under the
/// root. A rewrite of `attrs.json` moves a listing's modified time.
#[test]
fn list_returns_pending_publishes_in_id_order() {
    let f = fixture();
    let staging = Staging::with_root(&f.store, &f.publish_root);
    assert!(staging.list().unwrap().is_empty());

    let a = staging.init(&f.original).unwrap();
    let b = staging.init(&f.original).unwrap();
    fs::create_dir_all(f.publish_root.join("tmp").join("publish")).unwrap();
    fs::create_dir_all(f.publish_root.join("not-an-id")).unwrap();

    let listed = staging.list().unwrap();
    let mut expected = vec![a.id.clone(), b.id.clone()];
    expected.sort_unstable();
    let ids: Vec<&str> = listed.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, expected);
    assert!(listed.iter().all(|p| p.attrs.origin == a.origin));
    assert!(listed.iter().all(|p| p.dir == f.publish_root.join(&p.id)));
    let external = staging.external().unwrap();
    assert_eq!(
        external.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        expected
    );

    let before = staging.get(&a.id).unwrap().attrs_modified_ms;
    let later = SystemTime::now() + Duration::from_secs(90);
    fs::File::options()
        .write(true)
        .open(staging.layout(&a.id).session().attrs_file())
        .unwrap()
        .set_modified(later)
        .unwrap();
    let after = staging.get(&a.id).unwrap().attrs_modified_ms;
    assert!(after > before, "{before} -> {after}");
    assert_eq!(
        after,
        later.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
    );
}

/// `get` resolves a full id or a unique prefix, reports no match,
/// and reports an ambiguous prefix with its candidates.
#[test]
fn get_resolves_a_pending_publish_by_id_or_unique_prefix() {
    let f = fixture();
    let staging = Staging::with_root(&f.store, &f.publish_root);
    let a = staging.init(&f.original).unwrap();
    let b = staging.init(&f.original).unwrap();

    assert_eq!(staging.get(&a.id).unwrap().id, a.id);
    // Ids are random, so find a prefix of `a` that `b` does not share
    let unique =
        a.id.char_indices()
            .map(|(i, _)| &a.id[..=i])
            .find(|p| !b.id.starts_with(p))
            .unwrap();
    assert_eq!(staging.get(unique).unwrap().id, a.id);

    let missing = staging.get("zzzz").unwrap_err();
    assert!(
        matches!(missing, PublishError::Store(StoreError::ObjectNotFound(ref p)) if p == "zzzz"),
        "{missing}"
    );

    let ambiguous = staging.get("").unwrap_err();
    assert!(
        matches!(ambiguous, PublishError::Store(StoreError::AmbiguousId(_, ref c)) if c.len() == 2),
        "{ambiguous}"
    );
}

/// `apply` commits the staged session as it stands, keeps the
/// definition's values as the `publish-def` attachment targeting the
/// new session, written as comment-free JSON that reads back as the
/// same definition, and deletes the staging directory with the
/// temporary attachment files. The session does not link the
/// attachment. A second apply is `ObjectNotFound`.
#[test]
fn apply_commits_the_staged_session_and_attaches_the_definition() {
    let f = fixture();
    let staging = Staging::with_root(&f.store, &f.publish_root);
    let init = staging.init(&f.original).unwrap();
    let layout = staging.layout(&init.id);
    let session = layout.session();
    fs::write(
        session.files_dir().join("session.jsonl"),
        SESSION.lines().next().map(|l| format!("{l}\n")).unwrap(),
    )
    .unwrap();
    let changes = "[\n  [\"line\",2,\"scope\",\"user:test\"]\n]\n";
    fs::write(session.changes_file(), changes).unwrap();
    let mut definition = PublishDefinition::new(&f.original.id, &[]);
    definition.push_redact("world", "pii", "user:test", false);
    fs::write(layout.definition_file(), definition.to_jsonc_text(None)).unwrap();
    let staged_attrs = fs::read_to_string(session.attrs_file()).unwrap();

    let commit = staging.apply(&init.id, Some(&definition)).unwrap();

    let sessions = SessionStore::from(&f.store);
    let published = sessions.at_commit(&commit).unwrap();
    assert_eq!(published.id, init.id);
    assert_eq!(published.attrs.origin, init.origin);
    assert_eq!(
        serde_json::to_string(&published.attrs).unwrap(),
        serde_json::to_string(&serde_json::from_str::<SessionAttrsRecord>(&staged_attrs).unwrap())
            .unwrap()
    );
    let committed = sessions.changes(&commit).unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!((committed[0].line, committed[0].line_end), (2, Some(2)));
    assert_eq!(sessions.iter().unwrap().count(), 2);
    assert!(!f.publish_root.join(&init.id).exists());
    assert!(
        !f.publish_root
            .join("tmp")
            .join("publish")
            .join(&init.id)
            .exists()
    );
    assert!(staging.list().unwrap().is_empty());

    let attachments = AttachmentStore::from(&f.store);
    let kept = attachments
        .query()
        .name(DEFINITION_ATTACHMENT)
        .iter()
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(kept.len(), 1);
    let kept = &kept[0];
    assert_eq!(kept.attrs.key, None);
    assert_eq!(kept.attrs.targets, [format!("session:{}", init.id)]);
    let text = attachments
        .read_file(&kept.commit_sha, "publish.json")
        .unwrap()
        .unwrap();
    let text = String::from_utf8(text).unwrap();
    assert!(!text.contains("//"), "{text}");
    assert!(text.starts_with("{\n  "), "{text}");
    assert!(text.ends_with("}\n"), "{text}");
    let read_back: PublishDefinition = serde_json::from_str(&text).unwrap();
    assert_eq!(read_back, definition);
    let parents = f.store.read_commit(&kept.commit_sha).unwrap().parents;
    assert!(parents.contains(&commit));
    let closure: Vec<String> = f
        .store
        .link_closure(std::slice::from_ref(&init.id))
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(closure, std::slice::from_ref(&init.id));

    let again = staging.apply(&init.id, Some(&definition)).unwrap_err();
    assert!(
        matches!(again, PublishError::Store(StoreError::ObjectNotFound(ref id)) if *id == init.id),
        "{again}"
    );
}

/// Without a definition, `apply` commits the session and attaches
/// nothing.
#[test]
fn apply_without_a_definition_attaches_nothing() {
    let f = fixture();
    let staging = Staging::with_root(&f.store, &f.publish_root);
    let init = staging.init(&f.original).unwrap();

    let commit = staging.apply(&init.id, None).unwrap();

    assert_eq!(
        SessionStore::from(&f.store).at_commit(&commit).unwrap().id,
        init.id
    );
    assert_eq!(AttachmentStore::from(&f.store).iter().unwrap().count(), 0);
    assert!(!f.publish_root.join(&init.id).exists());
}

/// `apply` over a session the store holds writes that session again:
/// staged content equal to the stored commit writes nothing and
/// returns that commit. `apply` over a removed session is refused and
/// leaves the staging directory in place.
#[test]
fn apply_writes_an_existing_session_again_and_refuses_a_removed_one() {
    let f = fixture();
    let staging = Staging::with_root(&f.store, &f.publish_root);
    let init = staging.init(&f.original).unwrap();
    // Stage further publishes under the id of the first, as copies of
    // its directory, after the first is applied
    let twins = [f.publish_root.join("twin1"), f.publish_root.join("twin2")];
    for twin in &twins {
        copy_dir(&f.publish_root.join(&init.id), twin);
    }
    let first = staging.apply(&init.id, None).unwrap();
    let sessions = SessionStore::from(&f.store);

    fs::rename(&twins[0], f.publish_root.join(&init.id)).unwrap();
    assert_eq!(staging.apply(&init.id, None).unwrap(), first);
    assert!(!f.publish_root.join(&init.id).exists());
    assert_eq!(sessions.iter().unwrap().count(), 2);

    sessions.remove(std::slice::from_ref(&init.id)).unwrap();
    fs::rename(&twins[1], f.publish_root.join(&init.id)).unwrap();
    let removed = staging.apply(&init.id, None).unwrap_err();
    assert!(
        matches!(removed, PublishError::Store(StoreError::ObjectDeleted(ref id)) if *id == init.id),
        "{removed}"
    );
    assert!(f.publish_root.join(&init.id).is_dir());
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

/// `abort` deletes the staging directory, the definition with it, and
/// nothing else; a second abort of the same id is `ObjectNotFound`.
#[test]
fn abort_deletes_the_staging_directory() {
    let f = fixture();
    let staging = Staging::with_root(&f.store, &f.publish_root);
    let a = staging.init(&f.original).unwrap();
    let b = staging.init(&f.original).unwrap();
    fs::write(staging.definition_file(&a.id), b"{}\n").unwrap();
    fs::write(staging.definition_file(&b.id), b"{}\n").unwrap();

    staging.abort(&a.id).unwrap();
    assert!(!f.publish_root.join(&a.id).exists());
    assert!(!staging.definition_file(&a.id).exists());
    assert!(f.publish_root.join(&b.id).exists());
    assert!(staging.definition_file(&b.id).exists());
    assert_eq!(staging.list().unwrap().len(), 1);
    assert_eq!(SessionStore::from(&f.store).iter().unwrap().count(), 1);

    let again = staging.abort(&a.id).unwrap_err();
    assert!(
        matches!(again, PublishError::Store(StoreError::ObjectNotFound(ref id)) if *id == a.id),
        "{again}"
    );
}
