//! Where the session viewer reads sessions and writes notes.
//!
//! A [`Backend`] is a gage-query context over a native source or
//! over the store. Session and entry rows come from the context's
//! `session` and `entry` relations in either case; the native
//! relations are the `native_session(src)` and `native_entry(src)`
//! table functions. Notes target stored sessions, so only the store
//! backend reads or writes them.

use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use datafusion::arrow::array::{
    Array, Int64Array, RecordBatch, StringArray, TimestampMillisecondArray, UInt64Array,
};
use datafusion::arrow::json::ArrayWriter;
use datafusion::prelude::SessionContext;
use gage_query::ContextBuilder;
use gage_registry::driver::DriverRegistry;
use gage_session::Driver;
use gage_session::is_system;
use gage_store::url::{self, LineSelection};
use gage_store::{NoteEdit, NoteFull, NoteInput, NoteStore, NoteValue, RedactedKind, Store};
use ratatui::crossterm::terminal;
use serde_json::Value;

use crate::doc::{Document, Entry, Note, Redaction, Session};
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
    /// driver's default location. The returned builder opens the
    /// backend with [`NativeBackend::open`].
    pub fn native(spec: &str) -> NativeBackend {
        NativeBackend {
            spec: spec.to_string(),
            show_agents: false,
            cache_root: None,
        }
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
        let turns = self.load_turns(session_id, &entries).await?;
        // A native session is never a publish, and its context has no
        // `redacted` relation
        let (notes, redactions) = match self.kind {
            Kind::Native { .. } => (Vec::new(), Vec::new()),
            Kind::Stored { .. } => (
                self.load_notes(session_id).await?,
                self.load_redactions(session_id).await?,
            ),
        };
        Ok(Document {
            session,
            entries,
            turns,
            notes,
            redactions,
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
        let row = batches.iter().find(|b| b.num_rows() > 0);
        let mut value = row
            .map(first_row_as_value)
            .transpose()?
            .unwrap_or(Value::Null);
        nest_summary(&mut value);
        let pending = row.is_some_and(is_pending_publish);
        Ok(Session {
            id: session_id.to_string(),
            value,
            pending,
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
            "SELECT e.line, e.uuid, e.type, m.subtype, e.raw, m.text \
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
            let texts = column::<StringArray>(batch, 5);
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
                    message_text: texts.is_valid(i).then(|| texts.value(i).to_string()),
                    value,
                });
            }
        }
        Ok(entries)
    }

    /// The turn of every entry, aligned with `entries`. A turn is one
    /// API response: the assistant rows that share a response id,
    /// numbered by the line their response first appears on. A row of
    /// another type, or an assistant row without a response id, has
    /// none.
    async fn load_turns(
        &self,
        session_id: &str,
        entries: &[Entry],
    ) -> Result<Vec<Option<usize>>, Box<dyn Error>> {
        let id = sql_str(session_id);
        let sql = format!(
            "WITH a AS (SELECT line, json_extract(raw, '$.message.id') AS mid \
                        FROM {} WHERE session_id = '{id}' AND type = 'assistant'), \
                  f AS (SELECT mid, MIN(line) AS first_line FROM a \
                        WHERE mid IS NOT NULL GROUP BY mid), \
                  t AS (SELECT mid, DENSE_RANK() OVER (ORDER BY first_line) AS turn FROM f) \
             SELECT a.line, t.turn FROM a JOIN t ON a.mid = t.mid",
            self.entry_rel(),
        );
        let batches = self.ctx.sql(&sql).await?.collect().await?;
        let mut by_line: HashMap<u32, usize> = HashMap::new();
        for batch in &batches {
            let lines = column::<Int64Array>(batch, 0);
            let turns = column::<UInt64Array>(batch, 1);
            for i in 0..batch.num_rows() {
                by_line.insert(
                    u32::try_from(lines.value(i))?,
                    usize::try_from(turns.value(i))?,
                );
            }
        }
        Ok(entries
            .iter()
            .map(|e| by_line.get(&e.line).copied())
            .collect())
    }

    /// The records of a stored session's `redacted.json`, in original
    /// line order; none for a session that is not a publish.
    async fn load_redactions(&self, session_id: &str) -> Result<Vec<Redaction>, Box<dyn Error>> {
        let sql = format!(
            "SELECT kind, line, line_end, property, span_start, span_end, reason, author \
             FROM redacted WHERE session_id = '{}' ORDER BY line, property, span_start",
            sql_str(session_id)
        );
        let batches = self.ctx.sql(&sql).await?.collect().await?;
        let mut redactions = Vec::new();
        for batch in &batches {
            let kinds = column::<StringArray>(batch, 0);
            let lines = column::<Int64Array>(batch, 1);
            let line_ends = column::<Int64Array>(batch, 2);
            let properties = column::<StringArray>(batch, 3);
            let span_starts = column::<Int64Array>(batch, 4);
            let span_ends = column::<Int64Array>(batch, 5);
            let reasons = column::<StringArray>(batch, 6);
            let authors = column::<StringArray>(batch, 7);
            for i in 0..batch.num_rows() {
                let kind = match kinds.value(i) {
                    "line" => RedactedKind::Line,
                    "span" => RedactedKind::Span,
                    "value" => RedactedKind::Value,
                    other => return Err(format!("unknown redacted kind {other:?}").into()),
                };
                let line_at = |col: &Int64Array| -> Result<Option<u32>, Box<dyn Error>> {
                    Ok(if col.is_valid(i) {
                        Some(u32::try_from(col.value(i))?)
                    } else {
                        None
                    })
                };
                let offset_at = |col: &Int64Array| -> Result<Option<u64>, Box<dyn Error>> {
                    Ok(if col.is_valid(i) {
                        Some(u64::try_from(col.value(i))?)
                    } else {
                        None
                    })
                };
                redactions.push(Redaction {
                    kind,
                    line: u32::try_from(lines.value(i))?,
                    line_end: line_at(line_ends)?,
                    property: properties
                        .is_valid(i)
                        .then(|| properties.value(i).to_string()),
                    span_start: offset_at(span_starts)?,
                    span_end: offset_at(span_ends)?,
                    reason: reasons.value(i).to_string(),
                    author: authors.value(i).to_string(),
                });
            }
        }
        Ok(redactions)
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
        let (time_col, driver_col, title_col) = match self.kind {
            Kind::Native { .. } => ("mtime", "'' AS driver", "title"),
            Kind::Stored { .. } => (
                "modified",
                "driver",
                "json_extract(summary, '$.title') AS title",
            ),
        };
        let sql = format!(
            "SELECT id, id_display, project, {title_col}, {time_col}, {driver_col} \
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

/// A native backend before it is opened: the source, whether the
/// runtime's own in-flight agent sessions are read, and where the
/// source keeps its derived state.
pub struct NativeBackend {
    spec: String,
    show_agents: bool,
    cache_root: Option<PathBuf>,
}

impl NativeBackend {
    /// Read the runtime's own in-flight agent sessions too. The live
    /// scan view opens a running agent through such a backend.
    pub fn show_agents(mut self) -> Self {
        self.show_agents = true;
        self
    }

    /// Keep the source's derived state under `root` rather than the
    /// default cache root.
    pub fn cache_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.cache_root = Some(root.into());
        self
    }

    pub async fn open(self) -> Result<Backend, Box<dyn Error>> {
        let registry = DriverRegistry::builtin();
        let driver = registry
            .driver_for(&self.spec)
            .ok_or_else(|| format!("no session driver for source {:?}", self.spec))?;
        let mut ctx = ContextBuilder::new(None);
        if self.show_agents {
            ctx = ctx.show_agents();
        }
        if let Some(root) = self.cache_root {
            ctx = ctx.cache_root(root);
        }
        Ok(Backend {
            ctx: ctx.build().await,
            kind: Kind::Native {
                spec: self.spec,
                driver,
            },
            registry: Arc::new(registry),
        })
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
/// Whether the session row's first entry is a pending publish working
/// tree: its `locator` is the staging directory, where a stored
/// session's is `git:<commit>`. A row without a locator is not pending.
fn is_pending_publish(batch: &RecordBatch) -> bool {
    let Ok(idx) = batch.schema().index_of("locator") else {
        return false;
    };
    let locators = column::<StringArray>(batch, idx);
    locators.is_valid(0) && !locators.value(0).starts_with("git:")
}

/// Replace the row's `summary` text with the JSON object it holds, so
/// the pane's YAML nests its keys. The stored `summary` column is a
/// text column that carries a JSON object; a value that does not
/// parse is left as text, which the pane still shows.
fn nest_summary(row: &mut Value) {
    let Some(summary) = row.get_mut("summary") else {
        return;
    };
    let Some(text) = summary.as_str() else {
        return;
    };
    if let Ok(parsed) = serde_json::from_str::<Value>(text) {
        *summary = parsed;
    }
}

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
        /// Where the source keeps its derived state, beside the store
        pub cache_root: PathBuf,
        _dirs: (TempDir, TempDir),
    }

    pub(crate) fn fixture() -> Fixture {
        fixture_with_content(concat!(
            r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/w/proj","message":{"role":"user","content":"hello there"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
        ))
    }

    /// The fixture over a session whose JSONL is `content`.
    pub(crate) fn fixture_with_content(content: &str) -> Fixture {
        let claude_root = tempfile::tempdir().unwrap();
        let native_id = "11111111-2222-3333-4444-555555555555";
        let project_dir = claude_root.path().join("projects").join("-w-proj");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join(format!("{native_id}.jsonl")), content).unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let path = store_root.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open(&path).unwrap();
        let cache_root = store_root.path().join("cache");
        let spec = format!("claude:{}", claude_root.path().display());
        let registry = DriverRegistry::builtin();
        let driver = registry.driver_for(&spec).unwrap();
        let source = driver
            .open_source(SourceOptions::new(&spec).cache_root(&cache_root))
            .unwrap();
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
            cache_root,
            _dirs: (claude_root, store_root),
        }
    }

    /// Turns number the assistant rows by API response: the rows of one
    /// response share a turn, and a row without a response id has none.
    /// The stored and native backends agree.
    #[tokio::test(flavor = "multi_thread")]
    async fn turns_group_assistant_rows_by_response_id() {
        let f = fixture_with_content(concat!(
            r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/w/proj","message":{"role":"user","content":"one"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"id":"msg_1","role":"assistant","model":"claude-x","content":[{"type":"text","text":"a"}]}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a2","timestamp":"2025-01-01T00:00:02Z","message":{"id":"msg_1","role":"assistant","model":"claude-x","content":[{"type":"text","text":"b"}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"u2","timestamp":"2025-01-01T00:00:03Z","message":{"role":"user","content":"two"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a3","timestamp":"2025-01-01T00:00:04Z","message":{"id":"msg_2","role":"assistant","model":"claude-x","content":[{"type":"text","text":"c"}]}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a4","timestamp":"2025-01-01T00:00:05Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"text","text":"d"}]}}"#,
            "\n",
        ));
        let expected = vec![None, Some(1), Some(1), None, Some(2), None];

        let stored = Backend::stored(f.store).await;
        let doc = stored.load(&f.session_id).await.unwrap();
        assert_eq!(doc.entries.len(), 6);
        assert_eq!(doc.turns, expected);

        let native = Backend::native(&f.spec)
            .cache_root(&f.cache_root)
            .open()
            .await
            .unwrap();
        let doc = native.load(&f.native_id).await.unwrap();
        assert_eq!(doc.turns, expected);
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
        assert_eq!(doc.entries[0].message_text.as_deref(), Some("hello there"));
        assert_eq!(doc.entries[1].message_text.as_deref(), Some("hi"));
        let origin = doc.session.value["origin"].as_str().unwrap();
        assert!(origin.ends_with(&f.native_id), "{origin}");
        assert!(doc.session.value.get("locator").is_none());
        let summary = doc.session.value["summary"]
            .as_object()
            .expect("the summary column should load as a nested object");
        assert!(summary.contains_key("message_count"));
        assert!(!doc.session.pending);
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
        let backend = Backend::native(&f.spec)
            .cache_root(&f.cache_root)
            .open()
            .await
            .unwrap();
        assert!(backend.is_native());
        assert!(!backend.supports_notes());

        let doc = backend.load(&f.native_id).await.unwrap();
        assert!(
            f.cache_root.is_dir(),
            "the source's cache is under the given root"
        );
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

    /// A pending publish row locates its staging directory; a stored
    /// row locates a commit; a row without a locator is not pending.
    #[test]
    fn pending_publish_is_told_by_the_locator() {
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        let with = |locator: &str| {
            let schema = Arc::new(Schema::new(vec![
                Field::new("id", DataType::Utf8, false),
                Field::new("locator", DataType::Utf8, false),
            ]));
            RecordBatch::try_new(
                schema,
                vec![
                    Arc::new(StringArray::from(vec!["s1"])),
                    Arc::new(StringArray::from(vec![locator])),
                ],
            )
            .unwrap()
        };
        assert!(is_pending_publish(&with("/home/u/.gage/publish/s1")));
        assert!(!is_pending_publish(&with("git:0123abcd")));
        let without = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, false)])),
            vec![Arc::new(StringArray::from(vec!["s1"]))],
        )
        .unwrap();
        assert!(!is_pending_publish(&without));
    }
}
