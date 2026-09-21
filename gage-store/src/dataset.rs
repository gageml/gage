//! Dataset objects: `gage::dataset 1`, reached through [`DatasetStore`].
//!
//! Content is a `sessions.link` file listing the commit SHA of each
//! member session in insertion order; the position in that file is the
//! session number used by dataset session commands. Every listed SHA
//! is a commit parent, so pushing a dataset carries its sessions along.
//! Session content lives in [`gage::session`](crate::session) objects;
//! the dataset does not copy it. Tree construction, commit parents,
//! and edits are the generic object model's job; see
//! [`crate::object`].

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};

use gage_core::uuid::new_uuid;
use gage_session::{ContentSource, Driver, NativeSession};

use crate::git::{git_in, run};
use crate::index::{ObjectQuery, Order};
use crate::object::{EditOutcome, Object, ObjectTree};
use crate::session::{SessionAddOutcome, SessionOutcome, SessionStore};
use crate::{Store, StoreError};

pub(crate) const OBJECT_TYPE: &str = "gage::dataset";
const OBJECT_VERSION: &str = "1";
/// Datasets declare no indexed attributes.
pub(crate) const INDEXED_ATTRS: &[&str] = &[];
const SESSIONS_LINK: &str = "sessions.link";

/// Dataset operations over an opened store.
pub struct DatasetStore<'a> {
    store: &'a Store,
}

impl<'a> From<&'a Store> for DatasetStore<'a> {
    fn from(store: &'a Store) -> Self {
        DatasetStore { store }
    }
}

/// A dataset summary row for the list view.
#[derive(Debug, PartialEq, Eq)]
pub struct DatasetRecord {
    pub id: String,
    pub created_ms: i64,
}

/// Metadata for one member session in a dataset, resolved through the
/// dataset's `sessions.link` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    /// 1-based index in `sessions.link`.
    pub session_num: u32,
    /// Native session id (`attrs.native_id`).
    pub native_id: String,
    pub driver_name: String,
    pub driver_version: String,
    /// Harness family (`attrs.session_type`).
    pub session_type: String,
    /// Driver-owned byte-layout string as returned by
    /// [`Driver::write_native`].
    pub content_format: String,
}

/// Summary of one member session in a dataset, for list views.
#[derive(Debug, PartialEq, Eq)]
pub struct DatasetSessionSummary {
    pub session_num: u32,
    /// Native session id.
    pub native_id: String,
    /// Harness family (`attrs.session_type`, e.g. `"claude"`).
    pub session_type: String,
    /// The driver's size of the session's own files, when reported.
    pub size: Option<u64>,
}

/// Outcome of adding one session to a dataset.
#[derive(Debug, PartialEq, Eq)]
pub struct DatasetSessionAddOutcome {
    /// Position in `sessions.link` after the operation.
    pub session_num: u32,
    /// Gage object id of the session.
    pub id: String,
    pub outcome: SessionOutcome,
}

/// One session to add to a dataset. The driver drives the serialization
/// via [`Driver::write_native`]; `session` supplies the native id,
/// type, and attributes.
pub struct SessionSpec<'a> {
    pub driver: &'a dyn Driver,
    pub session: &'a mut dyn NativeSession,
}

impl DatasetStore<'_> {
    /// Create an empty dataset. Returns the new id.
    pub fn create(&self) -> Result<String, StoreError> {
        let id = new_uuid();
        self.store.create(
            OBJECT_TYPE,
            OBJECT_VERSION,
            &id,
            &ObjectTree::default(),
            "dataset",
        )?;
        Ok(id)
    }

    /// Every live dataset, newest created first, read lazily.
    pub fn iter(
        &self,
    ) -> Result<impl Iterator<Item = Result<DatasetRecord, StoreError>> + '_, StoreError> {
        self.query().iter()
    }

    /// Start a selection over datasets.
    pub fn query(&self) -> DatasetQuery<'_> {
        DatasetQuery {
            store: self.store,
            query: ObjectQuery::new(OBJECT_TYPE),
        }
    }

    /// Resolve a full id or unique prefix to the dataset's full id.
    pub fn resolve_id(&self, id_or_prefix: &str) -> Result<String, StoreError> {
        Ok(self.current(id_or_prefix)?.header.id)
    }

    fn current(&self, id_or_prefix: &str) -> Result<Object, StoreError> {
        self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)
    }

    /// Read a member session's metadata, resolving `session_ref` as
    /// either a decimal `<n>` or a native session id.
    pub fn session_meta(
        &self,
        dataset_id: &str,
        session_ref: &str,
    ) -> Result<SessionMeta, StoreError> {
        let members = members(&self.current(dataset_id)?);
        let session_num = self.resolve_session_num(&members, session_ref)?;
        let commit_sha = members
            .get((session_num - 1) as usize)
            .expect("resolve_session_num returns a valid 1-based index");
        let record = SessionStore::from(self.store).at_commit(commit_sha)?;
        Ok(SessionMeta {
            session_num,
            native_id: record.attrs.native_id,
            driver_name: record.driver_name,
            driver_version: record.driver_version,
            session_type: record.attrs.session_type,
            content_format: record.attrs.content_format,
        })
    }

    fn resolve_session_num(
        &self,
        members: &[String],
        session_ref: &str,
    ) -> Result<u32, StoreError> {
        if let Ok(n) = session_ref.parse::<u32>() {
            if n == 0 || (n as usize) > members.len() {
                return Err(StoreError::SessionNotFound(session_ref.to_string()));
            }
            return Ok(n);
        }
        let sessions = SessionStore::from(self.store);
        for (idx, sha) in members.iter().enumerate() {
            let record = sessions.at_commit(sha)?;
            if record.attrs.native_id == session_ref {
                return Ok((idx + 1) as u32);
            }
        }
        Err(StoreError::SessionNotFound(session_ref.to_string()))
    }

    /// Build a [`ContentSource`] backed by git for the session at
    /// position `session_num` in the given dataset.
    pub fn session_content(
        &self,
        dataset_id: &str,
        session_num: u32,
    ) -> Result<Box<dyn ContentSource>, StoreError> {
        let members = members(&self.current(dataset_id)?);
        if session_num == 0 || (session_num as usize) > members.len() {
            return Err(StoreError::SessionNotFound(session_num.to_string()));
        }
        let session_commit = members
            .get((session_num - 1) as usize)
            .expect("bounds checked above")
            .clone();
        Ok(Box::new(GitContentSource {
            store_path: self.store.path().to_path_buf(),
            session_commit,
        }))
    }

    /// List sessions in the given dataset, ordered by ascending
    /// `session_num`.
    pub fn sessions_list(
        &self,
        dataset_id: &str,
    ) -> Result<Vec<DatasetSessionSummary>, StoreError> {
        let members = members(&self.current(dataset_id)?);
        let sessions = SessionStore::from(self.store);
        let mut out = Vec::with_capacity(members.len());
        for (idx, session_commit) in members.iter().enumerate() {
            let record = sessions.at_commit(session_commit)?;
            out.push(DatasetSessionSummary {
                session_num: (idx + 1) as u32,
                native_id: record.attrs.native_id,
                session_type: record.attrs.session_type,
                size: record.size,
            });
        }
        Ok(out)
    }

    /// Add or update one or more sessions in the given dataset in a
    /// single commit. Each spec is written as a session object
    /// (created, updated, or unchanged); the dataset's `sessions.link`
    /// file is rewritten to reference the resulting commit SHAs. When
    /// no member SHA changes, no dataset commit is written.
    pub fn sessions_add(
        &self,
        dataset_id: &str,
        mut specs: Vec<SessionSpec<'_>>,
    ) -> Result<Vec<DatasetSessionAddOutcome>, StoreError> {
        if specs.is_empty() {
            return Ok(Vec::new());
        }
        let dataset = self.current(dataset_id)?;
        let mut members = members(&dataset);

        // Object ids of existing members, so an update to a session
        // already in the dataset replaces its slot instead of appending.
        let mut member_ids: Vec<String> = Vec::with_capacity(members.len());
        for sha in &members {
            member_ids.push(self.store.read_object(sha)?.header.id);
        }

        let sessions = SessionStore::from(self.store);
        let mut outcomes: Vec<DatasetSessionAddOutcome> = Vec::with_capacity(specs.len());
        for spec in specs.iter_mut() {
            let SessionAddOutcome {
                id,
                commit_sha,
                outcome,
            } = sessions.add(spec.driver, spec.session)?;
            let session_num = match member_ids.iter().position(|m| m == &id) {
                Some(idx) => {
                    *members
                        .get_mut(idx)
                        .expect("idx came from member_ids, which parallels members") = commit_sha;
                    (idx + 1) as u32
                }
                None => {
                    members.push(commit_sha);
                    member_ids.push(id.clone());
                    members.len() as u32
                }
            };
            outcomes.push(DatasetSessionAddOutcome {
                session_num,
                id,
                outcome,
            });
        }

        let mut tree = ObjectTree::default();
        tree.links.insert(SESSIONS_LINK.to_string(), members);
        let message = format_session_commit_message(&outcomes);
        match self.store.edit(&dataset, &tree, &message)? {
            EditOutcome::Unchanged | EditOutcome::Written(_) => Ok(outcomes),
        }
    }
}

/// A selection over datasets: an order and a limit. `iter` reads
/// matching datasets one at a time.
pub struct DatasetQuery<'a> {
    store: &'a Store,
    query: ObjectQuery,
}

impl<'a> DatasetQuery<'a> {
    pub fn order(mut self, order: Order) -> Self {
        self.query.order = order;
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.query.limit = Some(limit);
        self
    }

    /// Run the selection.
    pub fn iter(
        self,
    ) -> Result<impl Iterator<Item = Result<DatasetRecord, StoreError>> + 'a, StoreError> {
        let store = self.store;
        let tips = store.select(&self.query)?;
        Ok(tips.into_iter().map(move |tip| {
            let object = store.read_object(&tip.sha)?;
            let created_ms = object.header.created_ms.ok_or_else(|| {
                StoreError::Parse(format!("dataset {}: missing created", object.header.id))
            })?;
            Ok(DatasetRecord {
                id: object.header.id,
                created_ms,
            })
        }))
    }
}

fn members(object: &Object) -> Vec<String> {
    object
        .tree
        .links
        .get(SESSIONS_LINK)
        .cloned()
        .unwrap_or_default()
}

fn format_session_commit_message(outcomes: &[DatasetSessionAddOutcome]) -> String {
    let mut added: Vec<u32> = Vec::new();
    let mut updated: Vec<u32> = Vec::new();
    for o in outcomes {
        match o.outcome {
            SessionOutcome::Added => added.push(o.session_num),
            SessionOutcome::Updated => updated.push(o.session_num),
            SessionOutcome::Unchanged => {}
        }
    }
    added.sort();
    updated.sort();
    let mut parts: Vec<String> = Vec::new();
    if !added.is_empty() {
        parts.push(format!("add {}", join_nums(&added)));
    }
    if !updated.is_empty() {
        parts.push(format!("update {}", join_nums(&updated)));
    }
    format!("sessions: {}", parts.join("; "))
}

fn join_nums(nums: &[u32]) -> String {
    nums.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

struct GitContentSource {
    store_path: PathBuf,
    session_commit: String,
}

impl ContentSource for GitContentSource {
    fn paths(&self) -> io::Result<Vec<String>> {
        // `-z` prints names raw with NUL separators. Without it git
        // C-quotes any name with a byte >= 0x80, a tab, a backslash, a
        // quote, or a newline, and the quoted form is not a path.
        let listing = run(git_in(
            &self.store_path,
            [
                "ls-tree",
                "-r",
                "-z",
                "--name-only",
                &format!("{}:files.d", self.session_commit),
            ],
        ))
        .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(listing
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect())
    }

    fn open(&self, path: &str) -> io::Result<Box<dyn Read + Send>> {
        let target = format!("{}:files.d/{}", self.session_commit, path);
        let mut cmd: Command = git_in(&self.store_path, ["cat-file", "-p", &target]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .expect("stdout was requested via Stdio::piped");
        Ok(Box::new(GitReader {
            child,
            stdout,
            finished: false,
        }))
    }
}

struct GitReader {
    child: Child,
    stdout: ChildStdout,
    /// Set once the process has been waited on at end of stream.
    finished: bool,
}

impl Read for GitReader {
    /// At end of stream the process is reaped, and a failing status
    /// becomes an error carrying git's stderr, so a truncated read is
    /// never mistaken for a complete one.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.stdout.read(buf)?;
        if n == 0 && !self.finished {
            self.finished = true;
            let status = self.child.wait()?;
            if !status.success() {
                let mut reason = String::new();
                if let Some(stderr) = self.child.stderr.as_mut() {
                    // Best-effort: the stderr snapshot may be truncated
                    // or empty. The exit status in the error message
                    // carries git's own reason on its own.
                    drop(stderr.read_to_string(&mut reason));
                }
                return Err(io::Error::other(format!(
                    "git cat-file {status}: {}",
                    reason.trim()
                )));
            }
        }
        Ok(n)
    }
}

impl Drop for GitReader {
    fn drop(&mut self) {
        // Reap the git process to avoid a zombie. A reader dropped
        // before end of stream has nothing to report.
        if !self.finished {
            drop(self.child.kill());
            drop(self.child.wait());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::object_ref;
    use crate::test_support::open_store;
    use crate::{NoteInput, NoteStore};
    use gage_session::{ContentSink, DriverError, SessionAttrs, Source, StoredSession};
    use std::any::Any;
    use std::io::{Cursor, Write as _};
    use std::time::SystemTime;

    struct FakeSession {
        id: String,
        source: String,
        files: Vec<(String, String)>,
        attrs: FakeAttrs,
    }

    struct FakeAttrs {
        size: u64,
    }

    impl SessionAttrs for FakeAttrs {
        fn mtime(&self) -> Option<SystemTime> {
            None
        }
        fn size(&self) -> Option<u64> {
            Some(self.size)
        }
        fn is_empty(&self) -> Option<bool> {
            None
        }
        fn project_name(&self) -> Option<&str> {
            None
        }
        fn title(&self) -> Option<&str> {
            None
        }
        fn model(&self) -> Option<&str> {
            None
        }
        fn message_count(&self) -> Option<u64> {
            None
        }
    }

    impl NativeSession for FakeSession {
        fn native_id(&self) -> &str {
            &self.id
        }

        fn session_type(&self) -> &str {
            "fake"
        }

        fn source(&self) -> &str {
            &self.source
        }

        fn attrs(&self) -> &dyn SessionAttrs {
            &self.attrs
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
        fn open_source(&self, _source: &str) -> Result<Box<dyn Source>, DriverError> {
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
            for (path, content) in &fake.files {
                let mut w = sink.create(path).map_err(DriverError::Io)?;
                w.write_all(content.as_bytes()).map_err(DriverError::Io)?;
            }
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

    fn fake(id: &str, content: &str) -> FakeSession {
        fake_files(id, &[("session.jsonl", content)])
    }

    fn fake_files(id: &str, files: &[(&str, &str)]) -> FakeSession {
        let entries: Vec<(String, String)> = files
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect();
        let size = entries.iter().map(|(_, c)| c.len() as u64).sum();
        FakeSession {
            id: id.to_string(),
            source: format!("fake:{id}"),
            files: entries,
            attrs: FakeAttrs { size },
        }
    }

    fn add(
        store: &Store,
        dataset: &str,
        sessions: &mut [FakeSession],
    ) -> Vec<DatasetSessionAddOutcome> {
        let driver = FakeDriver;
        let specs = sessions
            .iter_mut()
            .map(|s| SessionSpec {
                driver: &driver,
                session: s,
            })
            .collect();
        DatasetStore::from(store)
            .sessions_add(dataset, specs)
            .unwrap()
    }

    // Silence unused-import warning; Cursor is used in the store's own
    // tests, not here.
    #[allow(dead_code)]
    const _: fn() = || {
        let _ = Cursor::new(Vec::<u8>::new());
    };

    fn rev_parse(store: &Store, id: &str) -> String {
        store.rev_parse(&object_ref(id)).unwrap().unwrap()
    }

    fn note(store: &Store) -> String {
        NoteStore::from(store)
            .create(NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            })
            .unwrap()
    }

    #[test]
    fn create_writes_ref_and_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let id = DatasetStore::from(&store).create().unwrap();
        assert_eq!(id.len(), 26);

        let ref_path = object_ref(&id);
        let listing = run(git_in(store.path(), ["ls-tree", "--name-only", &ref_path])).unwrap();
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            vec!["created", "id", "modified", "type"]
        );
        let type_content = run(git_in(
            store.path(),
            ["cat-file", "-p", &format!("{ref_path}:type")],
        ))
        .unwrap();
        assert_eq!(type_content, "gage::dataset 1\n");
        let id_content = run(git_in(
            store.path(),
            ["cat-file", "-p", &format!("{ref_path}:id")],
        ))
        .unwrap();
        assert_eq!(id_content, format!("{id}\n"));
    }

    #[test]
    fn list_reports_datasets_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let datasets = DatasetStore::from(&store);

        let a = datasets.create().unwrap();
        let b = datasets.create().unwrap();
        note(&store);

        let records: Vec<DatasetRecord> =
            datasets.iter().unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(records.len(), 2);
        let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&a.as_str()));
        assert!(ids.contains(&b.as_str()));
        for r in &records {
            assert!(r.created_ms > 0);
        }
    }

    #[test]
    fn iter_of_empty_store_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        assert_eq!(DatasetStore::from(&store).iter().unwrap().count(), 0);
    }

    #[test]
    fn resolve_id_rejects_other_types() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let note = note(&store);
        assert!(matches!(
            DatasetStore::from(&store).resolve_id(&note).unwrap_err(),
            StoreError::WrongType { actual, .. } if actual == "gage::note"
        ));
    }

    #[test]
    fn sessions_add_links_members_as_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let datasets = DatasetStore::from(&store);
        let dataset = datasets.create().unwrap();
        let before = rev_parse(&store, &dataset);

        let outcomes = add(
            &store,
            &dataset,
            &mut [fake("s1", "a\n"), fake("s2", "b\n")],
        );
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].session_num, 1);
        assert_eq!(outcomes[1].session_num, 2);
        assert!(outcomes.iter().all(|o| o.outcome == SessionOutcome::Added));

        let tip = rev_parse(&store, &dataset);
        let meta = store.read_commit(&tip).unwrap();
        let s1 = rev_parse(&store, &outcomes[0].id);
        let s2 = rev_parse(&store, &outcomes[1].id);
        assert_eq!(meta.parents, vec![before.clone(), s1.clone(), s2.clone()]);
        assert!(
            meta.message.starts_with("sessions: add 1, 2"),
            "{}",
            meta.message
        );

        let link = run(git_in(
            store.path(),
            [
                "cat-file",
                "-p",
                &format!("{}:sessions.link", object_ref(&dataset)),
            ],
        ))
        .unwrap();
        assert_eq!(link, format!("{s1}\n{s2}\n"));

        let listed = datasets.sessions_list(&dataset).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].native_id, "s1");
        assert_eq!(listed[1].native_id, "s2");
        assert_eq!(listed[0].session_type, "fake");
    }

    #[test]
    fn sessions_add_unchanged_writes_no_dataset_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let dataset = DatasetStore::from(&store).create().unwrap();
        add(&store, &dataset, &mut [fake("s1", "a\n")]);
        let tip = rev_parse(&store, &dataset);

        let outcomes = add(&store, &dataset, &mut [fake("s1", "a\n")]);
        assert_eq!(outcomes[0].outcome, SessionOutcome::Unchanged);
        assert_eq!(rev_parse(&store, &dataset), tip);
    }

    #[test]
    fn sessions_add_updates_slot_of_grown_member() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let datasets = DatasetStore::from(&store);
        let dataset = datasets.create().unwrap();
        add(
            &store,
            &dataset,
            &mut [fake("s1", "a\n"), fake("s2", "b\n")],
        );

        let outcomes = add(&store, &dataset, &mut [fake("s1", "a\nmore\n")]);
        assert_eq!(outcomes[0].outcome, SessionOutcome::Updated);
        assert_eq!(outcomes[0].session_num, 1);

        let listed = datasets.sessions_list(&dataset).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].size, Some(7));
        let meta = datasets.session_meta(&dataset, "s1").unwrap();
        assert_eq!(meta.session_num, 1);
        assert_eq!(meta.driver_name, "fake");
        assert!(matches!(
            datasets.session_meta(&dataset, "3").unwrap_err(),
            StoreError::SessionNotFound(s) if s == "3"
        ));
    }

    #[test]
    fn session_content_reads_files() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let datasets = DatasetStore::from(&store);
        let dataset = datasets.create().unwrap();
        add(&store, &dataset, &mut [fake("s1", "hello\n")]);

        let access = datasets.session_content(&dataset, 1).unwrap();
        assert_eq!(access.paths().unwrap(), vec!["session.jsonl"]);
        let mut text = String::new();
        access
            .open("session.jsonl")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello\n");
    }

    #[test]
    fn session_content_paths_are_raw_names() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let datasets = DatasetStore::from(&store);
        let dataset = datasets.create().unwrap();
        // Each of these is C-quoted by ls-tree without -z
        let files = [
            ("subagents/agent-\u{e9}.jsonl", "e"),
            ("subagents/back\\slash.txt", "b"),
            ("tab\there.txt", "t"),
            ("quo\"te.txt", "q"),
        ];
        add(&store, &dataset, &mut [fake_files("s1", &files)]);

        let access = datasets.session_content(&dataset, 1).unwrap();
        let mut paths = access.paths().unwrap();
        paths.sort();
        let mut expected: Vec<&str> = files.iter().map(|(p, _)| *p).collect();
        expected.sort();
        assert_eq!(paths, expected);
        for (path, content) in files {
            let mut text = String::new();
            access
                .open(path)
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            assert_eq!(text, content, "{path:?}");
        }
    }
}
