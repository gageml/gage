//! Where the session viewer reads sessions and writes notes.
//!
//! A [`Backend`] is a gage-query context over a native source or
//! over the store. Session and entry rows come from the context's
//! `session` and `entry` relations in either case; the native
//! relations are the `native_session(src)` and `native_entry(src)`
//! table functions. Notes target stored sessions, so only the store
//! backend reads or writes them.

use std::error::Error;
use std::sync::{Arc, Mutex, MutexGuard};

use datafusion::arrow::array::{
    Array, Int64Array, RecordBatch, StringArray, TimestampMillisecondArray,
};
use datafusion::arrow::json::ArrayWriter;
use datafusion::prelude::SessionContext;
use gage_query::ContextBuilder;
use gage_registry::driver::DriverRegistry;
use gage_session::Driver;
use gage_session::is_system;
use gage_store::url::{self, LineSelection};
use gage_store::{NoteEdit, NoteFull, NoteInput, NoteStore, NoteValue, Store};
use ratatui::crossterm::terminal;
use serde_json::Value;

use crate::doc::{Document, Entry, Note, Session};
use crate::format;

#[derive(Clone)]
pub struct Backend {
    ctx: SessionContext,
    kind: Kind,
    registry: Arc<DriverRegistry>,
}

#[derive(Clone)]
enum Kind {
    /// A driver source: sessions are native and carry no notes
    Native {
        /// The `--source` value; empty is the default location
        spec: String,
        driver: Arc<dyn Driver>,
    },
    /// The store: sessions are objects and notes target them
    Stored { store: Arc<Mutex<Store>> },
}

/// Width the picker gives a project name
const PROJECT_WIDTH: usize = 24;

/// Fewest sessions a picker page holds
const PICKER_PAGE_MIN: usize = 50;

/// Sessions per page of the session-open picker for the current
/// terminal. The floor applies when the terminal size is unavailable,
/// as when stdout is not a terminal.
pub fn picker_page_size() -> usize {
    match terminal::size() {
        Ok((_cols, rows)) => page_size_for_rows(rows),
        Err(_) => PICKER_PAGE_MIN,
    }
}

/// Two screens of picker rows, never fewer than the floor. The picker
/// frame takes ten of the terminal's lines: a two-line margin above
/// and below, two borders, one line of padding, the header, a gap,
/// and the key hint.
fn page_size_for_rows(rows: u16) -> usize {
    let visible = usize::from(rows.saturating_sub(10));
    (visible * 2).max(PICKER_PAGE_MIN)
}

impl Backend {
    /// Over the driver source `spec` names. Empty names the default
    /// driver's default location.
    pub async fn native(spec: &str) -> Result<Backend, Box<dyn Error>> {
        Self::native_showing(spec, false).await
    }

    /// Over the driver source `spec` names, reading the runtime's own
    /// in-flight agent sessions too. The live scan view opens a
    /// running agent through this backend.
    pub async fn native_with_agents(spec: &str) -> Result<Backend, Box<dyn Error>> {
        Self::native_showing(spec, true).await
    }

    async fn native_showing(spec: &str, show_agents: bool) -> Result<Backend, Box<dyn Error>> {
        let registry = DriverRegistry::builtin();
        let driver = registry
            .driver_for(spec)
            .ok_or_else(|| format!("no session driver for source {spec:?}"))?;
        let mut ctx = ContextBuilder::new(None);
        if show_agents {
            ctx = ctx.show_agents();
        }
        Ok(Backend {
            ctx: ctx.build().await,
            kind: Kind::Native {
                spec: spec.to_string(),
                driver,
            },
            registry: Arc::new(registry),
        })
    }

    /// Over the store.
    pub async fn stored(store: Store) -> Backend {
        Self::shared(Arc::new(Mutex::new(store))).await
    }

    /// Over a store handle the caller shares with other readers.
    pub async fn shared(store: Arc<Mutex<Store>>) -> Backend {
        Backend {
            ctx: ContextBuilder::new(Some(Arc::clone(&store))).build().await,
            kind: Kind::Stored { store },
            registry: Arc::new(DriverRegistry::builtin()),
        }
    }

    /// True over a native source, whose ids are the driver's
    pub fn is_native(&self) -> bool {
        matches!(self.kind, Kind::Native { .. })
    }

    /// True when notes can be read and written: the store backend
    pub fn supports_notes(&self) -> bool {
        matches!(self.kind, Kind::Stored { .. })
    }

    pub async fn load(&self, session_id: &str) -> Result<Document, Box<dyn Error>> {
        let session = self.load_session(session_id).await?;
        // The entries are read as the harness wrote them; a session
        // that exists must be in the one format this crate reads
        if let Some(content_format) = session.value.get("content_format").and_then(Value::as_str) {
            format::require_supported(session_id, content_format);
        }
        let entries = self.load_entries(session_id).await?;
        let notes = match self.kind {
            Kind::Native { .. } => Vec::new(),
            Kind::Stored { .. } => self.load_notes(session_id).await?,
        };
        Ok(Document {
            session,
            entries,
            notes,
        })
    }

    /// The session row without its system columns, as the `<Session>`
    /// pane shows it.
    async fn load_session(&self, session_id: &str) -> Result<Session, Box<dyn Error>> {
        let sql = format!(
            "SELECT * FROM {} WHERE id = '{}'",
            self.session_rel(),
            sql_str(session_id)
        );
        let batches = self.ctx.sql(&sql).await?.collect().await?;
        let value = batches
            .iter()
            .find(|b| b.num_rows() > 0)
            .map(|b| first_row_as_value(b))
            .transpose()?
            .unwrap_or(Value::Null);
        Ok(Session {
            id: session_id.to_string(),
            value,
        })
    }

    /// Every entry with the message subtype the driver derived for
    /// it, when the entry is a message.
    ///
    /// The session id is a literal on both sides of the join. A filter
    /// on the preserved side of a left join is not propagated to the
    /// other side, so an unfiltered message relation scans every
    /// session: 15 s against a store of 198k messages versus 0.4 s
    /// with the literal in the join condition.
    async fn load_entries(&self, session_id: &str) -> Result<Vec<Entry>, Box<dyn Error>> {
        let id = sql_str(session_id);
        let sql = format!(
            "SELECT e.line, e.uuid, e.type, m.subtype, e.raw \
             FROM {} e LEFT JOIN {} m ON m.session_id = '{id}' AND m.line = e.line \
             WHERE e.session_id = '{id}' ORDER BY e.line",
            self.entry_rel(),
            self.message_rel(),
        );
        let batches = self.ctx.sql(&sql).await?.collect().await?;
        let mut entries = Vec::new();
        for batch in &batches {
            let lines = column::<Int64Array>(batch, 0);
            let uuids = column::<StringArray>(batch, 1);
            let types = column::<StringArray>(batch, 2);
            let subtypes = column::<StringArray>(batch, 3);
            let raws = column::<StringArray>(batch, 4);
            for i in 0..batch.num_rows() {
                // A driver that supplies no raw row leaves the entry
                // with nothing to render; it still holds its line
                let value = if raws.is_null(i) {
                    Value::Null
                } else {
                    serde_json::from_str(raws.value(i))?
                };
                entries.push(Entry {
                    line: u32::try_from(lines.value(i)).unwrap_or(0),
                    uuid: uuids.is_valid(i).then(|| uuids.value(i).to_string()),
                    entry_type: types.value(i).to_string(),
                    message_subtype: subtypes.is_valid(i).then(|| subtypes.value(i).to_string()),
                    value,
                });
            }
        }
        Ok(entries)
    }

    /// The notes targeting the session, oldest first. The target URL
    /// is `session:<id>` with an optional line fragment, so a prefix
    /// match on the URL selects them in every scope.
    async fn load_notes(&self, session_id: &str) -> Result<Vec<Note>, Box<dyn Error>> {
        let target = format!("session:{}", sql_str(session_id));
        let sql = format!(
            "SELECT id, name, value, text, author, target, metadata, created, modified \
             FROM note WHERE target = '{target}' OR target LIKE '{target}#%' \
             ORDER BY created"
        );
        let batches = self.ctx.sql(&sql).await?.collect().await?;
        let mut notes = Vec::new();
        for batch in &batches {
            let ids = column::<StringArray>(batch, 0);
            let names = column::<StringArray>(batch, 1);
            let values = column::<StringArray>(batch, 2);
            let texts = column::<StringArray>(batch, 3);
            let authors = column::<StringArray>(batch, 4);
            let targets = column::<StringArray>(batch, 5);
            let metadatas = column::<StringArray>(batch, 6);
            let createds = column::<TimestampMillisecondArray>(batch, 7);
            let modifieds = column::<TimestampMillisecondArray>(batch, 8);
            for i in 0..batch.num_rows() {
                let value = if texts.is_valid(i) {
                    NoteValue::Text(texts.value(i).to_string())
                } else {
                    NoteValue::Json(serde_json::from_str(values.value(i))?)
                };
                let (line, line_end) = if targets.is_valid(i) {
                    target_lines(targets.value(i))?
                } else {
                    (None, None)
                };
                let metadata = if metadatas.is_valid(i) {
                    Some(serde_json::from_str(metadatas.value(i))?)
                } else {
                    None
                };
                notes.push(Note {
                    id: ids.value(i).to_string(),
                    name: names.value(i).to_string(),
                    value,
                    author: authors.value(i).to_string(),
                    line,
                    line_end,
                    metadata,
                    created_ms: timestamp_or_zero(createds, i),
                    modified_ms: timestamp_or_zero(modifieds, i),
                });
            }
        }
        Ok(notes)
    }

    /// One page of recent sessions, newest first, for the session-open
    /// picker: `limit` rows starting `offset` rows in.
    pub async fn list_recent(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SessionListItem>, Box<dyn Error>> {
        let (time_col, driver_col) = match self.kind {
            Kind::Native { .. } => ("mtime", "'' AS driver"),
            Kind::Stored { .. } => ("modified", "driver"),
        };
        let sql = format!(
            "SELECT id, id_display, project, title, {time_col}, {driver_col} \
             FROM {} ORDER BY {time_col} DESC LIMIT {limit} OFFSET {offset}",
            self.session_rel()
        );
        let batches = self.ctx.sql(&sql).await?.collect().await?;
        let mut items = Vec::new();
        for batch in &batches {
            let ids = column::<StringArray>(batch, 0);
            let displays = column::<StringArray>(batch, 1);
            let projects = column::<StringArray>(batch, 2);
            let titles = column::<StringArray>(batch, 3);
            let times = column::<TimestampMillisecondArray>(batch, 4);
            let drivers = column::<StringArray>(batch, 5);
            for i in 0..batch.num_rows() {
                let driver = self.row_driver(drivers.value(i))?;
                let project = if projects.is_valid(i) {
                    driver.format_project(projects.value(i), PROJECT_WIDTH)
                } else {
                    String::new()
                };
                items.push(SessionListItem {
                    id: ids.value(i).to_string(),
                    id_display: displays.value(i).to_string(),
                    project,
                    title: string_or_empty(titles, i),
                    time_ms: timestamp_or_zero(times, i),
                });
            }
        }
        Ok(items)
    }

    /// The driver that names a listed row's project: the source's
    /// driver for a native row, the recorded driver for a stored one.
    fn row_driver(&self, driver_col: &str) -> Result<Arc<dyn Driver>, Box<dyn Error>> {
        match &self.kind {
            Kind::Native { driver, .. } => Ok(Arc::clone(driver)),
            Kind::Stored { .. } => {
                // The store records `"<name> <version>"`
                let name = driver_col
                    .split_once(' ')
                    .map_or(driver_col, |(name, _)| name);
                self.registry
                    .for_name(name)
                    .ok_or_else(|| format!("unknown session driver '{name}'").into())
            }
        }
    }

    /// Write a note on the session, on `line` when given. Returns the
    /// note as stored.
    pub fn add_note(
        &self,
        session_id: &str,
        line: Option<u32>,
        name: &str,
        text: String,
        author: &str,
    ) -> Result<Note, Box<dyn Error>> {
        let store = self.store()?;
        let target = match line {
            Some(line) => format!("session:{session_id}#{line}"),
            None => format!("session:{session_id}"),
        };
        let notes = NoteStore::from(&*store);
        let id = notes.create(NoteInput {
            name,
            value: NoteValue::Text(text),
            author,
            target: Some(&target),
            metadata: None,
            work_id: None,
        })?;
        note_from_full(notes.get(&id)?)
    }

    /// Replace a note's value. Returns the note as stored.
    pub fn edit_note(&self, note_id: &str, text: String) -> Result<Note, Box<dyn Error>> {
        let store = self.store()?;
        let notes = NoteStore::from(&*store);
        notes.edit(
            note_id,
            NoteEdit {
                value: Some(NoteValue::Text(text)),
                ..NoteEdit::default()
            },
        )?;
        note_from_full(notes.get(note_id)?)
    }

    pub fn delete_note(&self, note_id: &str) -> Result<(), Box<dyn Error>> {
        let store = self.store()?;
        NoteStore::from(&*store).delete(note_id)?;
        Ok(())
    }

    fn store(&self) -> Result<MutexGuard<'_, Store>, Box<dyn Error>> {
        match &self.kind {
            Kind::Stored { store } => Ok(store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())),
            Kind::Native { .. } => Err("notes require a stored session".into()),
        }
    }

    fn session_rel(&self) -> String {
        match &self.kind {
            Kind::Native { spec, .. } => format!("native_session('{}')", sql_str(spec)),
            Kind::Stored { .. } => "session".to_string(),
        }
    }

    fn entry_rel(&self) -> String {
        match &self.kind {
            Kind::Native { spec, .. } => format!("native_entry('{}')", sql_str(spec)),
            Kind::Stored { .. } => "entry".to_string(),
        }
    }

    fn message_rel(&self) -> String {
        match &self.kind {
            Kind::Native { spec, .. } => format!("native_message('{}')", sql_str(spec)),
            Kind::Stored { .. } => "message".to_string(),
        }
    }
}

/// A row in the session-open picker.
pub struct SessionListItem {
    pub id: String,
    /// The short display form of `id`
    pub id_display: String,
    /// The project as the driver displays it; empty when unrecorded
    pub project: String,
    pub title: String,
    /// The native mtime for a source, the store's `modified` marker
    /// for the store
    pub time_ms: i64,
}

fn note_from_full(full: NoteFull) -> Result<Note, Box<dyn Error>> {
    let (line, line_end) = match &full.target {
        Some(target) => target_lines(target)?,
        None => (None, None),
    };
    Ok(Note {
        id: full.id,
        name: full.name,
        value: full.value,
        author: full.author,
        line,
        line_end,
        metadata: full.metadata,
        created_ms: full.created_ms,
        modified_ms: full.modified_ms,
    })
}

/// The first range of a target URL's line selection: `#12` gives
/// `(12, None)`, `#12-20,31` gives `(12, 20)`, no fragment gives
/// `(None, None)`.
fn target_lines(target: &str) -> Result<(Option<u32>, Option<u32>), Box<dyn Error>> {
    let Some(fragment) = url::parse(target)?.fragment else {
        return Ok((None, None));
    };
    let selection: LineSelection = fragment.parse()?;
    let Some(&(start, end)) = selection.ranges().first() else {
        return Ok((None, None));
    };
    let start = u32::try_from(start)?;
    let end = u32::try_from(end)?;
    Ok((Some(start), (start != end).then_some(end)))
}

/// The row's non-system columns as a JSON object, for the YAML pane.
fn first_row_as_value(batch: &RecordBatch) -> Result<Value, Box<dyn Error>> {
    let keep: Vec<usize> = batch
        .schema()
        .fields()
        .iter()
        .enumerate()
        .filter(|(_, f)| !is_system(f))
        .map(|(i, _)| i)
        .collect();
    let row = batch.slice(0, 1).project(&keep)?;
    let mut buf: Vec<u8> = Vec::new();
    let mut writer = ArrayWriter::new(&mut buf);
    writer.write(&row)?;
    writer.finish()?;
    let arr: Vec<Value> = serde_json::from_slice(&buf)?;
    Ok(arr.into_iter().next().unwrap_or(Value::Null))
}

fn column<T: 'static>(batch: &RecordBatch, idx: usize) -> &T {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<T>()
        .expect("column type matches the session, entry, or note schema")
}

fn string_or_empty(col: &StringArray, i: usize) -> String {
    if col.is_valid(i) {
        col.value(i).to_string()
    } else {
        String::new()
    }
}

fn timestamp_or_zero(col: &TimestampMillisecondArray, i: usize) -> i64 {
    if col.is_valid(i) { col.value(i) } else { 0 }
}

/// Escape a value for a single-quoted SQL literal
fn sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
pub(crate) mod tests {
    use gage_session::SourceOptions;
    use gage_store::SessionStore;
    use tempfile::TempDir;

    use super::*;

    /// A native Claude session in a temporary source, also added to a
    /// temporary store: the source spec, the native id, the store,
    /// and the Gage session id. The temp dirs are returned to keep
    /// them alive.
    pub(crate) struct Fixture {
        pub spec: String,
        pub native_id: String,
        pub store: Store,
        pub session_id: String,
        _dirs: (TempDir, TempDir),
    }

    pub(crate) fn fixture() -> Fixture {
        let claude_root = tempfile::tempdir().unwrap();
        let native_id = "11111111-2222-3333-4444-555555555555";
        let project_dir = claude_root.path().join("projects").join("-w-proj");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join(format!("{native_id}.jsonl")),
            concat!(
                r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/w/proj","message":{"role":"user","content":"hello there"}}"#,
                "\n",
                r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"text","text":"hi"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let path = store_root.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open(&path).unwrap();
        let spec = format!("claude:{}", claude_root.path().display());
        let registry = DriverRegistry::builtin();
        let driver = registry.driver_for(&spec).unwrap();
        let source = driver.open_source(&SourceOptions::new(&spec)).unwrap();
        let mut native = source.open_native(native_id).unwrap();
        let session_id = SessionStore::from(&store)
            .add(driver.as_ref(), native.as_mut())
            .unwrap()
            .id;
        Fixture {
            spec,
            native_id: native_id.to_string(),
            store,
            session_id,
            _dirs: (claude_root, store_root),
        }
    }

    #[test]
    fn page_size_is_two_screens_with_a_floor() {
        assert_eq!(page_size_for_rows(24), 50);
        assert_eq!(page_size_for_rows(35), 50);
        assert_eq!(page_size_for_rows(36), 52);
        assert_eq!(page_size_for_rows(60), 100);
        assert_eq!(page_size_for_rows(0), 50);
    }

    #[test]
    fn target_lines_reads_the_first_range() {
        assert_eq!(target_lines("session:abc").unwrap(), (None, None));
        assert_eq!(target_lines("session:abc#12").unwrap(), (Some(12), None));
        assert_eq!(
            target_lines("session:abc#12-20,31").unwrap(),
            (Some(12), Some(20))
        );
    }

    #[test]
    fn target_lines_rejects_a_line_beyond_u32() {
        let beyond = u64::from(u32::MAX) + 1;
        assert!(target_lines(&format!("session:abc#{beyond}")).is_err());
        assert!(target_lines(&format!("session:abc#1-{beyond}")).is_err());
    }

    /// The store backend reads the session, its entries, and its
    /// notes anchored by line, and writes notes back through the
    /// store; the system columns stay out of the session value.
    #[tokio::test]
    async fn stored_backend_loads_and_writes_notes() {
        let f = fixture();
        let backend = Backend::stored(f.store).await;
        assert!(backend.supports_notes());
        assert!(!backend.is_native());

        let doc = backend.load(&f.session_id).await.unwrap();
        assert_eq!(doc.entries.len(), 2);
        assert_eq!(doc.entries[0].entry_type(), "user");
        assert_eq!(doc.session.value["native_id"], f.native_id);
        assert!(doc.session.value.get("locator").is_none());
        assert!(doc.notes.is_empty());

        let note = backend
            .add_note(
                &f.session_id,
                Some(2),
                "comment",
                "on line two".into(),
                "user:t",
            )
            .unwrap();
        assert_eq!((note.line, note.line_end), (Some(2), None));
        let whole = backend
            .add_note(
                &f.session_id,
                None,
                "comment",
                "whole session".into(),
                "user:t",
            )
            .unwrap();
        assert_eq!(whole.line, None);

        let doc = backend.load(&f.session_id).await.unwrap();
        assert_eq!(doc.notes.len(), 2);
        assert_eq!(doc.notes_for_line(2)[0].text(), "on line two");
        assert_eq!(doc.session_notes()[0].text(), "whole session");

        let edited = backend.edit_note(&note.id, "revised".into()).unwrap();
        assert_eq!(edited.text(), "revised");
        backend.delete_note(&whole.id).unwrap();
        let doc = backend.load(&f.session_id).await.unwrap();
        assert_eq!(doc.notes.len(), 1);
        assert_eq!(doc.notes[0].text(), "revised");

        let recent = backend.list_recent(10, 0).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].id, f.session_id);
    }

    /// The native backend reads the same session by native id, with
    /// no notes and the note writes refused.
    #[tokio::test]
    async fn native_backend_loads_without_notes() {
        let f = fixture();
        let backend = Backend::native(&f.spec).await.unwrap();
        assert!(backend.is_native());
        assert!(!backend.supports_notes());

        let doc = backend.load(&f.native_id).await.unwrap();
        assert_eq!(doc.entries.len(), 2);
        assert_eq!(doc.session.value["id"], f.native_id);
        assert!(doc.session.value.get("path").is_none());
        assert!(doc.notes.is_empty());
        assert!(
            backend
                .add_note(&f.native_id, None, "comment", "x".into(), "user:t")
                .is_err()
        );

        let recent = backend.list_recent(10, 0).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].id, f.native_id);
        // The driver shows a project it cannot resolve by its stored name
        assert_eq!(recent[0].project, "-w-proj");
    }
}
