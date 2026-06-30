use rusqlite::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::target::NoteTarget;

/// A note's value, stored in SQLite as canonical JSON text. The value
/// may be any JSON type — a scalar (bool, number, string) or a
/// structured object/array — so scanners can record their natural
/// result directly rather than smuggling it through `metadata`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoteValue(pub serde_json::Value);

impl NoteValue {
    /// Canonical JSON text. This is the value's string form wherever it
    /// is rendered for a human (CLI, MCP). The DataFusion provider does
    /// not use this — it passes the raw column text through via
    /// [`NoteRaw`].
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.0).expect("serde_json::Value serializes")
    }
}

impl ToSql for NoteValue {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Owned(rusqlite::types::Value::Text(
            self.to_json(),
        )))
    }
}

impl FromSql for NoteValue {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        serde_json::from_str(text)
            .map(NoteValue)
            .map_err(|e| rusqlite::types::FromSqlError::Other(Box::new(e)))
    }
}

impl From<serde_json::Value> for NoteValue {
    fn from(v: serde_json::Value) -> Self {
        NoteValue(v)
    }
}

impl From<&str> for NoteValue {
    fn from(s: &str) -> Self {
        NoteValue(serde_json::Value::String(s.to_string()))
    }
}

impl From<String> for NoteValue {
    fn from(s: String) -> Self {
        NoteValue(serde_json::Value::String(s))
    }
}

impl From<i64> for NoteValue {
    fn from(i: i64) -> Self {
        NoteValue(serde_json::Value::Number(i.into()))
    }
}

impl From<f64> for NoteValue {
    fn from(f: f64) -> Self {
        let n =
            serde_json::Number::from_f64(f).expect("note float is finite (JSON has no NaN/Inf)");
        NoteValue(serde_json::Value::Number(n))
    }
}

impl From<bool> for NoteValue {
    fn from(b: bool) -> Self {
        NoteValue(serde_json::Value::Bool(b))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub author: String,
    /// Epoch milliseconds.
    pub created: i64,
    /// Epoch milliseconds. `None` until the note is edited.
    pub modified: Option<i64>,
    /// Advisory target the note refers to. Stored as a URI; never used
    /// for queries (see `session_note` / `project_note` for that).
    pub target: NoteTarget,
    pub name: String,
    pub value: NoteValue,
    pub explanation: Option<String>,
    pub metadata: Option<String>,
}

/// Read-only projection of a note that keeps `value` and `metadata` as
/// their raw JSON text rather than decoding them. The DataFusion
/// provider uses this to pass JSON straight from SQLite into Arrow
/// `Utf8` columns without a parse/re-serialize round-trip.
#[derive(Debug, Clone)]
pub struct NoteRaw {
    pub id: String,
    pub author: String,
    pub created: i64,
    pub modified: Option<i64>,
    pub target: NoteTarget,
    pub name: String,
    pub value: String,
    pub explanation: Option<String>,
    pub metadata: Option<String>,
}

#[derive(Debug, Default)]
pub struct NoteFilters {
    /// Session-scoped filter (single session id).
    pub session: Option<String>,
    /// Session-scoped filter across multiple session ids (IN).
    pub sessions: Vec<String>,
    /// Scan-scoped filter; matches notes recorded in `scan_note` for the
    /// given scan id, regardless of the note's advisory target.
    pub scan: Option<String>,
    pub author: Option<String>,
    pub name: Option<String>,
    /// Scanner name filter; matches `author = scanner:{name}`.
    pub scanner: Option<String>,
}

#[derive(Debug)]
pub enum NoteError {
    NotFound(String),
    Ambiguous(String, Vec<String>),
    /// A note with the same `(name, target, author)` already exists. The
    /// existing note is returned so the caller can decide what to do.
    Duplicate(Box<Note>),
    /// A session target carried a `session_id` that is not the expected
    /// UUID form. There is no FK on `session_note.session_id` (sessions
    /// live in the filesystem, not a table), so this is the cheap
    /// shape check we use in place of one.
    InvalidSessionId(String),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for NoteError {
    fn from(e: rusqlite::Error) -> Self {
        NoteError::Db(e)
    }
}

impl std::fmt::Display for NoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoteError::NotFound(prefix) => write!(f, "no note matching '{prefix}'"),
            NoteError::Ambiguous(prefix, ids) => {
                write!(f, "Found more than one note matching {prefix}")?;
                for id in ids {
                    write!(f, "\n  {id}")?;
                }
                Ok(())
            }
            NoteError::Duplicate(prev) => {
                write!(
                    f,
                    "duplicate note (name={}, target={})",
                    prev.name,
                    prev.target.to_uri()
                )
            }
            NoteError::InvalidSessionId(id) => {
                write!(f, "invalid session id: '{id}' (expected 36-char UUID)")
            }
            NoteError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for NoteError {}

impl Note {
    /// Build a new note.
    ///
    /// A trailing `.` in `name` is expanded to an 8-char suffix derived from
    /// the generated id, so callers can ask for a unique name (e.g.
    /// `"comment."` → `"comment.abcd1234"`) without threading the id back
    /// through the caller.
    pub fn new(target: NoteTarget, name: &str, value: impl Into<NoteValue>, author: &str) -> Self {
        let id = gage_core::uuid::new_uuid();
        let name = if name.ends_with('.') {
            format!("{name}{}", gage_core::uuid::short_uuid(&id))
        } else {
            name.to_string()
        };
        Note {
            id,
            author: author.to_string(),
            created: gage_core::datetime::now_ms(),
            modified: None,
            target,
            name,
            value: value.into(),
            explanation: None,
            metadata: None,
        }
    }
}

const NOTE_COLUMNS: &str = "id, created, modified, author, target,
    name, value, explanation, metadata";

/// Insert a note.
///
/// Returns `NoteError::Duplicate(prev)` if a note with the same
/// `(name, target, author)` already exists; the existing note is left
/// untouched and returned so the caller can decide what to do.
pub fn insert(conn: &Connection, note: &Note) -> Result<(), NoteError> {
    let tx = conn.unchecked_transaction()?;
    let target_uri = note.target.to_uri();
    let insert_res = tx.execute(
        &format!(
            "INSERT INTO note ({NOTE_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
        ),
        params![
            note.id,
            note.created,
            note.modified,
            note.author,
            target_uri,
            note.name,
            note.value,
            note.explanation,
            note.metadata,
        ],
    );
    if let Err(e) = insert_res {
        if is_unique_violation(&e) {
            drop(tx);
            let prev = find_by_dup_key(conn, &note.name, &target_uri, &note.author)?;
            return Err(NoteError::Duplicate(Box::new(prev)));
        }
        return Err(e.into());
    }
    insert_target_relation(&tx, note)?;
    tx.commit()?;
    Ok(())
}

fn is_unique_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(err, _)
            if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    )
}

fn find_by_dup_key(
    conn: &Connection,
    name: &str,
    target_uri: &str,
    author: &str,
) -> Result<Note, NoteError> {
    let mut stmt = conn.prepare(&format!(
        "{NOTE_SELECT} WHERE n.name = ?1 AND n.target = ?2 AND n.author = ?3"
    ))?;
    stmt.query_row(params![name, target_uri, author], row_to_note)
        .map_err(NoteError::from)
}

/// Replace an existing note's mutable fields (`value`, `metadata`,
/// `explanation`) and stamp `modified`. Identity
/// (`name`/`target`/`author`) is unchanged.
pub fn replace(conn: &Connection, prev_id: &str, new: &Note) -> Result<Note, NoteError> {
    let tx = conn.unchecked_transaction()?;
    let modified = gage_core::datetime::now_ms();
    let rows = tx.execute(
        "UPDATE note SET value = ?1, metadata = ?2, explanation = ?3, modified = ?4
         WHERE id = ?5",
        params![new.value, new.metadata, new.explanation, modified, prev_id],
    )?;
    if rows == 0 {
        return Err(NoteError::NotFound(prev_id.to_string()));
    }
    tx.commit()?;
    let mut stmt = conn.prepare(&format!("{NOTE_SELECT} WHERE n.id = ?1"))?;
    stmt.query_row([prev_id], row_to_note)
        .map_err(NoteError::from)
}

/// Mirror a note's advisory target into the relation table that backs
/// target-scoped queries. Scan targets are never filtered, so they have
/// no relation row.
fn insert_target_relation(conn: &Connection, note: &Note) -> Result<(), NoteError> {
    match &note.target {
        NoteTarget::Session(s) => {
            if !is_session_id_shape(&s.session_id) {
                return Err(NoteError::InvalidSessionId(s.session_id.clone()));
            }
            conn.execute(
                "INSERT INTO session_note (session_id, line, line_end, note_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![s.session_id, s.line, s.line_end, note.id],
            )?;
        }
        NoteTarget::Project(p) => {
            conn.execute(
                "INSERT INTO project_note (project_path, note_id) VALUES (?1, ?2)",
                params![p.project_path, note.id],
            )?;
        }
        NoteTarget::Scan(_) => {}
    }
    Ok(())
}

/// Cheap shape check: 36 chars and 4 hyphens at the UUID v4 positions.
/// Stand-in for a real FK; the canonical session lives in the filesystem.
fn is_session_id_shape(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 36
        && b.get(8) == Some(&b'-')
        && b.get(13) == Some(&b'-')
        && b.get(18) == Some(&b'-')
        && b.get(23) == Some(&b'-')
}

pub fn update(
    conn: &Connection,
    id: &str,
    value: &NoteValue,
    modified: i64,
) -> Result<(), NoteError> {
    let rows = conn.execute(
        "UPDATE note SET value = ?1, modified = ?2 WHERE id = ?3",
        params![value, modified, id],
    )?;
    if rows == 0 {
        return Err(NoteError::NotFound(id.to_string()));
    }
    Ok(())
}

pub fn delete(conn: &Connection, id: &str) -> Result<(), NoteError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM note_relation WHERE note_id = ?1 OR related_to = ?1",
        [id],
    )?;
    tx.execute("DELETE FROM session_note WHERE note_id = ?1", [id])?;
    tx.execute("DELETE FROM project_note WHERE note_id = ?1", [id])?;
    let rows = tx.execute("DELETE FROM note WHERE id = ?1", [id])?;
    if rows == 0 {
        return Err(NoteError::NotFound(id.to_string()));
    }
    tx.commit()?;
    Ok(())
}

const NOTE_SELECT: &str = "SELECT n.id, n.created, n.modified, n.author, n.target,
            n.name, n.value, n.explanation, n.metadata
     FROM note n";

pub fn get(conn: &Connection, id_prefix: &str) -> Result<Note, NoteError> {
    let pattern = format!("{id_prefix}%");
    let mut stmt = conn.prepare(&format!("{NOTE_SELECT} WHERE n.id LIKE ?1"))?;
    let notes: Vec<Note> = stmt
        .query_map([&pattern], row_to_note)?
        .collect::<Result<Vec<_>, _>>()?;
    match notes.len() {
        0 => Err(NoteError::NotFound(id_prefix.to_string())),
        1 => Ok(notes.into_iter().next().unwrap()),
        _ => {
            let mut ids: Vec<String> = notes.into_iter().map(|n| n.id).collect();
            ids.sort();
            Err(NoteError::Ambiguous(id_prefix.to_string(), ids))
        }
    }
}

pub fn related(conn: &Connection, note_id: &str) -> Result<Vec<Note>, NoteError> {
    let sql = format!(
        "{NOTE_SELECT}
         WHERE n.id IN (
             SELECT related_to FROM note_relation WHERE note_id = ?1
             UNION
             SELECT note_id FROM note_relation WHERE related_to = ?1
         )
         ORDER BY n.created ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let notes = stmt
        .query_map([note_id], row_to_note)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(notes)
}

fn find_query(filters: &NoteFilters) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut clauses = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(session) = &filters.session {
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM session_note sn
                     WHERE sn.note_id = n.id AND sn.session_id = ?{})",
            values.len() + 1
        ));
        values.push(Box::new(session.clone()));
    }
    if !filters.sessions.is_empty() {
        let start = values.len() + 1;
        let placeholders: Vec<String> = (0..filters.sessions.len())
            .map(|i| format!("?{}", start + i))
            .collect();
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM session_note sn
                     WHERE sn.note_id = n.id AND sn.session_id IN ({}))",
            placeholders.join(", ")
        ));
        for s in &filters.sessions {
            values.push(Box::new(s.clone()));
        }
    }
    if let Some(scan) = &filters.scan {
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM scan_note sx
                     WHERE sx.note_id = n.id AND sx.scan_id = ?{})",
            values.len() + 1
        ));
        values.push(Box::new(scan.clone()));
    }
    if let Some(author) = &filters.author {
        clauses.push(format!("n.author = ?{}", values.len() + 1));
        values.push(Box::new(author.clone()));
    }
    if let Some(name) = &filters.name {
        clauses.push(format!("n.name = ?{}", values.len() + 1));
        values.push(Box::new(name.clone()));
    }
    if let Some(scanner) = &filters.scanner {
        clauses.push(format!("n.author = ?{}", values.len() + 1));
        values.push(Box::new(format!("scanner:{scanner}")));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    let sql = format!("{NOTE_SELECT}{where_clause} ORDER BY n.created DESC");
    (sql, values)
}

pub fn find(conn: &Connection, filters: &NoteFilters) -> Result<Vec<Note>, NoteError> {
    let (sql, values) = find_query(filters);
    let params: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let notes = stmt
        .query_map(params.as_slice(), row_to_note)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(notes)
}

/// Total number of notes in the table.
pub fn count(conn: &Connection) -> Result<u32, NoteError> {
    let n: u32 = conn.query_row("SELECT COUNT(*) FROM note", [], |row| row.get(0))?;
    Ok(n)
}

pub fn find_raw(conn: &Connection, filters: &NoteFilters) -> Result<Vec<NoteRaw>, NoteError> {
    let (sql, values) = find_query(filters);
    let params: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let notes = stmt
        .query_map(params.as_slice(), row_to_note_raw)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(notes)
}

/// Parse the advisory `target` URI column into a `NoteTarget`, mapping
/// a malformed URI to a SQLite conversion error.
pub fn target_from_column(column: usize, uri: String) -> rusqlite::Result<NoteTarget> {
    NoteTarget::from_uri(&uri).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn row_to_note(row: &rusqlite::Row) -> rusqlite::Result<Note> {
    Ok(Note {
        id: row.get(0)?,
        created: row.get(1)?,
        modified: row.get(2)?,
        author: row.get(3)?,
        target: target_from_column(4, row.get(4)?)?,
        name: row.get(5)?,
        value: row.get(6)?,
        explanation: row.get(7)?,
        metadata: row.get(8)?,
    })
}

fn row_to_note_raw(row: &rusqlite::Row) -> rusqlite::Result<NoteRaw> {
    Ok(NoteRaw {
        id: row.get(0)?,
        created: row.get(1)?,
        modified: row.get(2)?,
        author: row.get(3)?,
        target: target_from_column(4, row.get(4)?)?,
        name: row.get(5)?,
        value: row.get(6)?,
        explanation: row.get(7)?,
        metadata: row.get(8)?,
    })
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::db::open_db_in_memory;
    use crate::target::{ProjectTarget, ScanTarget, SessionTarget};

    const SESSION_A: &str = "550e8400-e29b-41d4-a716-446655440000";
    const SESSION_B: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

    fn note_with(id: &str, name: &str, target: NoteTarget) -> Note {
        Note {
            id: id.to_string(),
            author: "user:test".to_string(),
            created: 1_742_428_800_000,
            modified: None,
            target,
            name: name.to_string(),
            value: NoteValue::from("test value"),
            explanation: None,
            metadata: None,
        }
    }

    fn session_target_of(session_id: &str) -> NoteTarget {
        NoteTarget::Session(SessionTarget::new(session_id))
    }

    fn line_target(session_id: &str, line: u32) -> NoteTarget {
        NoteTarget::Session(SessionTarget::new(session_id).with_line(line))
    }

    fn add_scan(conn: &Connection, id: &str) {
        conn.execute("INSERT INTO scan (id, created) VALUES (?1, 0)", [id])
            .unwrap();
    }

    #[test]
    fn insert_and_get() {
        let conn = open_db_in_memory().unwrap();
        let note = note_with(SESSION_A, "summary", session_target_of(SESSION_A));
        insert(&conn, &note).unwrap();

        let fetched = get(&conn, "550e8400").unwrap();
        assert_eq!(fetched.id, note.id);
        assert_eq!(fetched.name, "summary");
        match fetched.target {
            NoteTarget::Session(t) => assert_eq!(t.session_id, SESSION_A),
            other => panic!("expected session target, got {other:?}"),
        }
    }

    #[test]
    fn session_note_captures_line_range() {
        let conn = open_db_in_memory().unwrap();
        let note = note_with(
            "rng-1",
            "span",
            NoteTarget::Session(SessionTarget::new(SESSION_A).with_line_range(42, 50)),
        );
        insert(&conn, &note).unwrap();

        let (line, line_end): (u32, u32) = conn
            .query_row(
                "SELECT line, line_end FROM session_note WHERE note_id = 'rng-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((line, line_end), (42, 50));

        // round-trips back onto the note's target via the URI
        match get(&conn, "rng-1").unwrap().target {
            NoteTarget::Session(t) => {
                assert_eq!(t.line, Some(42));
                assert_eq!(t.line_end, Some(50));
            }
            other => panic!("expected session target, got {other:?}"),
        }
    }

    #[test]
    fn line_targets_persist_separately() {
        let conn = open_db_in_memory().unwrap();
        insert(
            &conn,
            &note_with("aaa-001", "score", line_target(SESSION_A, 42)),
        )
        .unwrap();
        insert(
            &conn,
            &note_with("aaa-002", "label", line_target(SESSION_A, 42)),
        )
        .unwrap();

        let count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_note WHERE session_id = ?1",
                [SESSION_A],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn insert_scan_target() {
        let conn = open_db_in_memory().unwrap();
        add_scan(&conn, "scan-1");
        let note = note_with(
            "bbb-001",
            "scan.finding",
            NoteTarget::Scan(ScanTarget {
                scan_id: "scan-1".to_string(),
            }),
        );
        insert(&conn, &note).unwrap();
        let fetched = get(&conn, "bbb-001").unwrap();
        match fetched.target {
            NoteTarget::Scan(t) => assert_eq!(t.scan_id, "scan-1"),
            other => panic!("expected scan target, got {other:?}"),
        }
    }

    #[test]
    fn insert_project_target() {
        let conn = open_db_in_memory().unwrap();
        let note = note_with(
            "ccc-001",
            "project.finding",
            NoteTarget::Project(ProjectTarget {
                project_path: "/home/me/proj".to_string(),
            }),
        );
        insert(&conn, &note).unwrap();
        let fetched = get(&conn, "ccc-001").unwrap();
        match fetched.target {
            NoteTarget::Project(t) => assert_eq!(t.project_path, "/home/me/proj"),
            other => panic!("expected project target, got {other:?}"),
        }
    }

    #[test]
    fn list_filter_by_session() {
        let conn = open_db_in_memory().unwrap();
        insert(
            &conn,
            &note_with(SESSION_A, "summary", session_target_of(SESSION_A)),
        )
        .unwrap();
        insert(
            &conn,
            &note_with(SESSION_B, "tag", session_target_of(SESSION_B)),
        )
        .unwrap();

        let notes = find(
            &conn,
            &NoteFilters {
                session: Some(SESSION_A.to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn list_filter_by_name() {
        let conn = open_db_in_memory().unwrap();
        insert(
            &conn,
            &note_with(SESSION_A, "summary", session_target_of(SESSION_A)),
        )
        .unwrap();
        insert(
            &conn,
            &note_with(SESSION_B, "tag", session_target_of(SESSION_B)),
        )
        .unwrap();

        let notes = find(
            &conn,
            &NoteFilters {
                name: Some("summary".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].name, "summary");
    }

    #[test]
    fn list_filter_by_scanner_matches_author() {
        let conn = open_db_in_memory().unwrap();
        let mut a = note_with(SESSION_A, "summary", session_target_of(SESSION_A));
        a.author = "scanner:user_friction".to_string();
        insert(&conn, &a).unwrap();
        let mut b = note_with(SESSION_B, "summary", session_target_of(SESSION_B));
        b.author = "scanner:other".to_string();
        insert(&conn, &b).unwrap();

        let notes = find(
            &conn,
            &NoteFilters {
                scanner: Some("user_friction".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].author, "scanner:user_friction");
    }

    #[test]
    fn delete_works() {
        let conn = open_db_in_memory().unwrap();
        insert(
            &conn,
            &note_with(SESSION_A, "x", session_target_of(SESSION_A)),
        )
        .unwrap();
        delete(&conn, SESSION_A).unwrap();
        assert!(matches!(get(&conn, SESSION_A), Err(NoteError::NotFound(_))));
    }

    #[test]
    fn duplicate_key_returns_prev_and_keeps_one_row() {
        let conn = open_db_in_memory().unwrap();
        let a = Note::new(
            session_target_of(SESSION_A),
            "summary",
            NoteValue::from("first"),
            "scanner:user_friction",
        );
        insert(&conn, &a).unwrap();

        // Same (name, target, author), fresh id and different value
        let b = Note::new(
            session_target_of(SESSION_A),
            "summary",
            NoteValue::from("second"),
            "scanner:user_friction",
        );
        match insert(&conn, &b) {
            Err(NoteError::Duplicate(prev)) => {
                assert_eq!(prev.id, a.id);
                assert_eq!(prev.value, NoteValue::from("first"));
            }
            other => panic!("expected Duplicate, got {other:?}"),
        }

        let n: u32 = conn
            .query_row("SELECT COUNT(*) FROM note", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn replace_updates_value() {
        let conn = open_db_in_memory().unwrap();

        let a = Note::new(
            session_target_of(SESSION_A),
            "summary",
            NoteValue::from("first"),
            "scanner:s",
        );
        insert(&conn, &a).unwrap();

        let b = Note::new(
            session_target_of(SESSION_A),
            "summary",
            NoteValue::from("second"),
            "scanner:s",
        );
        let prev = match insert(&conn, &b) {
            Err(NoteError::Duplicate(prev)) => prev,
            other => panic!("expected Duplicate, got {other:?}"),
        };

        let updated = replace(&conn, &prev.id, &b).unwrap();
        assert_eq!(updated.value, NoteValue::from("second"));
        assert!(updated.modified.is_some());
        assert_eq!(updated.id, a.id);
    }

    #[test]
    fn value_roundtrips_all_json_types() {
        let conn = open_db_in_memory().unwrap();
        let cases = [
            serde_json::json!({"fast": {"count": 5}, "ok": true}),
            serde_json::json!([1, 2, 3]),
            serde_json::json!(true),
            serde_json::json!(42),
            serde_json::json!(3.5),
            serde_json::json!("hello"),
            // JSON null serializes to the text "null", which satisfies
            // the column's NOT NULL constraint (it is not SQL NULL)
            serde_json::json!(null),
        ];
        for (i, json) in cases.into_iter().enumerate() {
            // Distinct name per case so each insert is a fresh note
            // under the (name, target, author) dedup key
            let note = Note::new(
                session_target_of(SESSION_A),
                &format!("n{i}"),
                NoteValue::from(json.clone()),
                "user:test",
            );
            insert(&conn, &note).unwrap();
            let fetched = get(&conn, &note.id).unwrap();
            assert_eq!(fetched.value, NoteValue(json));
        }
    }

    #[test]
    fn value_is_queryable_as_json_in_sqlite() {
        let conn = open_db_in_memory().unwrap();
        let note = Note::new(
            session_target_of(SESSION_A),
            "fast-mode.summary",
            NoteValue::from(serde_json::json!({"fast": {"count": 5}})),
            "user:test",
        );
        insert(&conn, &note).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT value ->> '$.fast.count' FROM note WHERE id = ?1",
                [&note.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 5);
    }

    #[test]
    fn find_raw_passes_through_raw_json_text() {
        let conn = open_db_in_memory().unwrap();
        let mut note = Note::new(
            session_target_of(SESSION_A),
            "n",
            NoteValue::from(serde_json::json!({"a": 1})),
            "user:test",
        );
        note.metadata = Some(r#"{"m":2}"#.to_string());
        insert(&conn, &note).unwrap();

        let raw = find_raw(&conn, &NoteFilters::default()).unwrap();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].value, r#"{"a":1}"#);
        assert_eq!(raw[0].metadata.as_deref(), Some(r#"{"m":2}"#));
    }
}
