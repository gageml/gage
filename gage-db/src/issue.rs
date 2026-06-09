use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::note::{Note, target_from_column};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueStatus {
    Open,
    Closed,
}

impl IssueStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueStatus::Open => "open",
            IssueStatus::Closed => "closed",
        }
    }
}

impl std::str::FromStr for IssueStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(IssueStatus::Open),
            "closed" => Ok(IssueStatus::Closed),
            other => Err(format!("unknown issue status '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClosedReason {
    Completed,
    Skipped,
}

impl ClosedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ClosedReason::Completed => "completed",
            ClosedReason::Skipped => "skipped",
        }
    }
}

impl std::str::FromStr for ClosedReason {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "completed" => Ok(ClosedReason::Completed),
            "skipped" => Ok(ClosedReason::Skipped),
            other => Err(format!("unknown closed_reason '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueEventKind {
    Create,
    Close,
    Reopen,
    Comment,
}

impl IssueEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueEventKind::Create => "create",
            IssueEventKind::Close => "close",
            IssueEventKind::Reopen => "reopen",
            IssueEventKind::Comment => "comment",
        }
    }
}

impl std::str::FromStr for IssueEventKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "create" => Ok(IssueEventKind::Create),
            "close" => Ok(IssueEventKind::Close),
            "reopen" => Ok(IssueEventKind::Reopen),
            "comment" => Ok(IssueEventKind::Comment),
            other => Err(format!("unknown issue event type '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub name: String,
    /// Advisory target URI the issue refers to, using the same scheme as
    /// `note.target`. Empty string for a global issue. Part of the
    /// duplication key `(name, target)`.
    pub target: String,
    pub title: String,
    pub description: Option<String>,
    pub status: IssueStatus,
    /// `Some` when `status == Closed`; `None` while `Open`.
    pub closed_reason: Option<ClosedReason>,
    /// Epoch milliseconds.
    pub created: i64,
    /// Epoch milliseconds. `None` until the issue is updated.
    pub modified: Option<i64>,
    /// Issue identity: `scanner:{name}` for scanner-written issues,
    /// `user:{name}` for issues added by a person. Used to resolve
    /// `scanner:{path}` URIs in issue fields. Not part of the
    /// duplication key.
    pub author: String,
}

/// A note recorded as evidence for an issue, linked via the
/// `issue_evidence` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueEvidence {
    pub issue_id: String,
    pub note_id: String,
    /// Evidence name, typically the note's name. Used to compare like
    /// evidence (ordering, change detection).
    pub name: String,
    /// Ever-increasing timestamp for evidence of the same name; typically
    /// epoch milliseconds.
    pub timestamp: i64,
    /// Optional digest used to detect evidence changes.
    pub digest: Option<String>,
}

/// A logged change to an issue, recorded in the `issue_event` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueEvent {
    pub issue_id: String,
    /// Stored in the `type` column.
    pub kind: IssueEventKind,
    pub author: String,
    /// Epoch milliseconds.
    pub timestamp: i64,
    /// Free-form value for the event, e.g. a close/reopen message.
    pub value: Option<String>,
}

#[derive(Debug)]
pub enum IssueError {
    NotFound(String),
    Ambiguous(String, Vec<String>),
    /// An issue with the same `(name, target)` already exists. The
    /// existing issue is returned so the caller can decide what to do.
    Duplicate(Box<Issue>),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for IssueError {
    fn from(e: rusqlite::Error) -> Self {
        IssueError::Db(e)
    }
}

impl std::fmt::Display for IssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IssueError::NotFound(prefix) => write!(f, "no issue matching '{prefix}'"),
            IssueError::Ambiguous(prefix, ids) => {
                write!(f, "Found more than one issue matching {prefix}")?;
                for id in ids {
                    write!(f, "\n  {id}")?;
                }
                Ok(())
            }
            IssueError::Duplicate(prev) => {
                write!(
                    f,
                    "duplicate issue (name={}, target={})",
                    prev.name, prev.target
                )
            }
            IssueError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for IssueError {}

const ISSUE_COLUMNS: &str =
    "id, name, target, title, description, status, closed_reason, created, modified, author";

/// Insert an issue.
///
/// Returns `IssueError::Duplicate(prev)` if an issue with the same
/// `(name, target)` already exists; the existing issue is left
/// untouched and returned so the caller can decide what to do.
pub fn insert(conn: &Connection, issue: &Issue) -> Result<(), IssueError> {
    let tx = conn.unchecked_transaction()?;
    let res = tx.execute(
        &format!(
            "INSERT INTO issue ({ISSUE_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
        ),
        params![
            issue.id,
            issue.name,
            issue.target,
            issue.title,
            issue.description,
            issue.status.as_str(),
            issue.closed_reason.map(ClosedReason::as_str),
            issue.created,
            issue.modified,
            issue.author,
        ],
    );
    if let Err(e) = res {
        if is_unique_violation(&e) {
            let prev = find_by_dup_key(conn, &issue.name, &issue.target)?;
            return Err(IssueError::Duplicate(Box::new(prev)));
        }
        return Err(e.into());
    }
    insert_event(
        &tx,
        &IssueEvent {
            issue_id: issue.id.clone(),
            kind: IssueEventKind::Create,
            author: issue.author.clone(),
            timestamp: issue.created,
            value: None,
        },
    )?;
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

fn find_by_dup_key(conn: &Connection, name: &str, target: &str) -> Result<Issue, IssueError> {
    let mut stmt = conn.prepare(&format!(
        "{ISSUE_SELECT} WHERE i.name = ?1 AND i.target = ?2"
    ))?;
    stmt.query_row(params![name, target], row_to_issue)
        .map_err(IssueError::from)
}

/// Reopen a closed issue: clears `closed_reason` and sets status back
/// to `open`. Bumps `modified` and logs a `Reopen` event carrying the
/// optional message. The update and event insert share a transaction.
pub fn reopen(
    conn: &Connection,
    issue_id: &str,
    author: &str,
    message: Option<&str>,
    timestamp: i64,
) -> Result<(), IssueError> {
    let tx = conn.unchecked_transaction()?;
    let rows = tx.execute(
        "UPDATE issue
         SET status = 'open', closed_reason = NULL, modified = ?1
         WHERE id = ?2",
        params![timestamp, issue_id],
    )?;
    if rows == 0 {
        return Err(IssueError::NotFound(issue_id.to_string()));
    }
    insert_event(
        &tx,
        &IssueEvent {
            issue_id: issue_id.to_string(),
            kind: IssueEventKind::Reopen,
            author: author.to_string(),
            timestamp,
            value: message.map(str::to_string),
        },
    )?;
    tx.commit()?;
    Ok(())
}

/// Mark an issue as closed with the given reason. Idempotent: a
/// repeat close overwrites `closed_reason` and bumps `modified`. Logs a
/// `Close` event carrying the optional message. The update and event
/// insert share a transaction.
pub fn close(
    conn: &Connection,
    issue_id: &str,
    reason: ClosedReason,
    author: &str,
    message: Option<&str>,
    timestamp: i64,
) -> Result<(), IssueError> {
    let tx = conn.unchecked_transaction()?;
    let rows = tx.execute(
        "UPDATE issue
         SET status = 'closed', closed_reason = ?1, modified = ?2
         WHERE id = ?3",
        params![reason.as_str(), timestamp, issue_id],
    )?;
    if rows == 0 {
        return Err(IssueError::NotFound(issue_id.to_string()));
    }
    insert_event(
        &tx,
        &IssueEvent {
            issue_id: issue_id.to_string(),
            kind: IssueEventKind::Close,
            author: author.to_string(),
            timestamp,
            value: message.map(str::to_string),
        },
    )?;
    tx.commit()?;
    Ok(())
}

/// Append an issue event.
pub fn insert_issue_event(conn: &Connection, event: &IssueEvent) -> Result<(), IssueError> {
    insert_event(conn, event)
}

fn insert_event(conn: &Connection, event: &IssueEvent) -> Result<(), IssueError> {
    conn.execute(
        "INSERT INTO issue_event (issue_id, type, author, timestamp, value)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            event.issue_id,
            event.kind.as_str(),
            event.author,
            event.timestamp,
            event.value,
        ],
    )?;
    Ok(())
}

/// Events logged against `issue_id`, ordered by `timestamp` ascending.
pub fn issue_events_for(conn: &Connection, issue_id: &str) -> Result<Vec<IssueEvent>, IssueError> {
    let mut stmt = conn.prepare(
        "SELECT issue_id, type, author, timestamp, value
         FROM issue_event WHERE issue_id = ?1 ORDER BY timestamp ASC",
    )?;
    let rows = stmt
        .query_map([issue_id], row_to_event)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn row_to_event(row: &rusqlite::Row) -> rusqlite::Result<IssueEvent> {
    let kind_str: String = row.get(1)?;
    let kind = kind_str.parse::<IssueEventKind>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(e)),
        )
    })?;
    Ok(IssueEvent {
        issue_id: row.get(0)?,
        kind,
        author: row.get(2)?,
        timestamp: row.get(3)?,
        value: row.get(4)?,
    })
}

/// Delete an issue and its `issue_evidence` links. Evidence notes are
/// not deleted — only the link rows go. Notes target
/// sessions/scans/projects, never an issue, so the issue owns no notes
/// of its own.
pub fn delete(conn: &Connection, issue_id: &str) -> Result<(), IssueError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM issue_evidence WHERE issue_id = ?1", [issue_id])?;
    let rows = tx.execute("DELETE FROM issue WHERE id = ?1", [issue_id])?;
    if rows == 0 {
        return Err(IssueError::NotFound(issue_id.to_string()));
    }
    tx.commit()?;
    Ok(())
}

pub fn insert_issue_evidence(conn: &Connection, ev: &IssueEvidence) -> Result<(), IssueError> {
    conn.execute(
        "INSERT INTO issue_evidence (issue_id, note_id, name, timestamp, digest)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![ev.issue_id, ev.note_id, ev.name, ev.timestamp, ev.digest],
    )?;
    Ok(())
}

pub fn list_issue_evidence(conn: &Connection) -> Result<Vec<IssueEvidence>, IssueError> {
    let mut stmt =
        conn.prepare("SELECT issue_id, note_id, name, timestamp, digest FROM issue_evidence")?;
    let rows = stmt
        .query_map([], row_to_evidence)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Evidence rows linked to `issue_id`, used to compare incoming evidence
/// against the record (newer timestamp, changed digest).
pub fn issue_evidence_for(
    conn: &Connection,
    issue_id: &str,
) -> Result<Vec<IssueEvidence>, IssueError> {
    let mut stmt = conn.prepare(
        "SELECT issue_id, note_id, name, timestamp, digest
         FROM issue_evidence WHERE issue_id = ?1",
    )?;
    let rows = stmt
        .query_map([issue_id], row_to_evidence)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn row_to_evidence(row: &rusqlite::Row) -> rusqlite::Result<IssueEvidence> {
    Ok(IssueEvidence {
        issue_id: row.get(0)?,
        note_id: row.get(1)?,
        name: row.get(2)?,
        timestamp: row.get(3)?,
        digest: row.get(4)?,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IssueStatusFilter {
    /// Only `open` issues (default).
    #[default]
    Open,
    /// Only `closed` issues.
    Closed,
    /// All issues regardless of status.
    Any,
}

#[derive(Debug, Default)]
pub struct IssueFilters {
    pub status: IssueStatusFilter,
    pub name: Option<String>,
}

const ISSUE_SELECT: &str = "SELECT i.id, i.name, i.target, i.title, i.description, i.status,
            i.closed_reason, i.created, i.modified, i.author
     FROM issue i";

pub fn find(conn: &Connection, filters: &IssueFilters) -> Result<Vec<Issue>, IssueError> {
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    match filters.status {
        IssueStatusFilter::Open => clauses.push("i.status = 'open'".to_string()),
        IssueStatusFilter::Closed => clauses.push("i.status = 'closed'".to_string()),
        IssueStatusFilter::Any => {}
    }
    if let Some(name) = &filters.name {
        clauses.push(format!("i.name = ?{}", values.len() + 1));
        values.push(Box::new(name.clone()));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    let sql = format!("{ISSUE_SELECT}{where_clause} ORDER BY i.created DESC");
    let params: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let issues = stmt
        .query_map(params.as_slice(), row_to_issue)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(issues)
}

pub fn get(conn: &Connection, id_prefix: &str) -> Result<Issue, IssueError> {
    let pattern = format!("{id_prefix}%");
    let mut stmt = conn.prepare(&format!("{ISSUE_SELECT} WHERE i.id LIKE ?1"))?;
    let issues: Vec<Issue> = stmt
        .query_map([&pattern], row_to_issue)?
        .collect::<Result<Vec<_>, _>>()?;
    match issues.len() {
        0 => Err(IssueError::NotFound(id_prefix.to_string())),
        1 => Ok(issues.into_iter().next().unwrap()),
        _ => {
            let mut ids: Vec<String> = issues.into_iter().map(|t| t.id).collect();
            ids.sort();
            Err(IssueError::Ambiguous(id_prefix.to_string(), ids))
        }
    }
}

/// Evidence notes linked to `issue_id`, ordered by evidence `timestamp`,
/// then `note.created`.
pub fn related_notes(conn: &Connection, issue_id: &str) -> Result<Vec<Note>, IssueError> {
    let sql = "SELECT n.id, n.created, n.modified, n.author, n.target,
                      n.name, n.value, n.explanation, n.metadata
               FROM issue_evidence ie
               JOIN note n ON ie.note_id = n.id
               WHERE ie.issue_id = ?1
               ORDER BY ie.timestamp ASC, n.created ASC";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([issue_id], row_to_note)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
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

fn row_to_issue(row: &rusqlite::Row) -> rusqlite::Result<Issue> {
    let status_str: String = row.get(5)?;
    let status = status_str.parse::<IssueStatus>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(e)),
        )
    })?;
    let closed_reason: Option<String> = row.get(6)?;
    let closed_reason = closed_reason
        .map(|s| {
            s.parse::<ClosedReason>().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other(e)),
                )
            })
        })
        .transpose()?;
    Ok(Issue {
        id: row.get(0)?,
        name: row.get(1)?,
        target: row.get(2)?,
        title: row.get(3)?,
        description: row.get(4)?,
        status,
        closed_reason,
        created: row.get(7)?,
        modified: row.get(8)?,
        author: row.get(9)?,
    })
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::db::open_db_in_memory;

    fn sample(id: &str, name: &str) -> Issue {
        Issue {
            id: id.to_string(),
            name: name.to_string(),
            target: String::new(),
            title: "Sample title".to_string(),
            description: Some("scanner:description.md".to_string()),
            status: IssueStatus::Open,
            closed_reason: None,
            created: 1_742_428_800_000,
            modified: None,
            author: "scanner:test".to_string(),
        }
    }

    #[test]
    fn insert_and_get() {
        let conn = open_db_in_memory();
        let issue = sample("issue-aaa", "thinking.empty");
        insert(&conn, &issue).unwrap();
        let fetched = get(&conn, "issue-aaa").unwrap();
        assert_eq!(fetched.name, "thinking.empty");
        assert_eq!(fetched.status, IssueStatus::Open);
        assert_eq!(
            fetched.description.as_deref(),
            Some("scanner:description.md")
        );
    }

    #[test]
    fn find_filters_resolved_by_default() {
        let conn = open_db_in_memory();
        let i1 = sample("issue-aaa", "n1");
        let mut i2 = sample("issue-bbb", "n2");
        i2.status = IssueStatus::Closed;
        i2.closed_reason = Some(ClosedReason::Completed);
        i2.created = i1.created + 1;
        insert(&conn, &i1).unwrap();
        insert(&conn, &i2).unwrap();

        let open = find(&conn, &IssueFilters::default()).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "issue-aaa");

        let all = find(
            &conn,
            &IssueFilters {
                status: IssueStatusFilter::Any,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(all.len(), 2);

        let closed = find(
            &conn,
            &IssueFilters {
                status: IssueStatusFilter::Closed,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, "issue-bbb");
        assert_eq!(closed[0].closed_reason, Some(ClosedReason::Completed));
    }

    #[test]
    fn duplicate_key_returns_prev_and_keeps_one_row() {
        let conn = open_db_in_memory();
        let a = sample("issue-aaa", "thinking.empty");
        insert(&conn, &a).unwrap();

        // Same (name, target), fresh id and different title.
        let mut b = sample("issue-bbb", "thinking.empty");
        b.title = "different".to_string();
        match insert(&conn, &b) {
            Err(IssueError::Duplicate(prev)) => assert_eq!(prev.id, "issue-aaa"),
            other => panic!("expected Duplicate, got {other:?}"),
        }

        let n: u32 = conn
            .query_row("SELECT COUNT(*) FROM issue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn same_name_distinct_target_is_not_duplicate() {
        let conn = open_db_in_memory();
        let mut a = sample("issue-aaa", "thinking.empty");
        a.target = "session:sess-1".to_string();
        insert(&conn, &a).unwrap();

        let mut b = sample("issue-bbb", "thinking.empty");
        b.target = "session:sess-2".to_string();
        insert(&conn, &b).unwrap();

        let n: u32 = conn
            .query_row("SELECT COUNT(*) FROM issue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn insert_issue_evidence_link() {
        use crate::note::{NoteValue, insert as insert_note};
        use crate::target::{NoteTarget, SessionTarget};

        let conn = open_db_in_memory();
        let issue = sample("issue-bbb", "thinking.empty");
        insert(&conn, &issue).unwrap();

        let note = Note {
            id: "note-1".to_string(),
            author: "user:test".to_string(),
            created: 1_742_428_800_000,
            modified: None,
            target: NoteTarget::Session(SessionTarget::new("sess-1")),
            name: "thinking.empty".to_string(),
            value: NoteValue::from("yes"),
            explanation: None,
            metadata: None,
        };
        insert_note(&conn, &note).unwrap();

        insert_issue_evidence(
            &conn,
            &IssueEvidence {
                issue_id: "issue-bbb".to_string(),
                note_id: "note-1".to_string(),
                name: "thinking.empty".to_string(),
                timestamp: 1_742_428_800_000,
                digest: None,
            },
        )
        .unwrap();

        let count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM issue_evidence WHERE issue_id = ?1",
                ["issue-bbb"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn close_logs_event_with_message() {
        let conn = open_db_in_memory();
        let issue = sample("issue-aaa", "thinking.empty");
        insert(&conn, &issue).unwrap();

        close(
            &conn,
            "issue-aaa",
            ClosedReason::Completed,
            "user:tester",
            Some("done in PR 42"),
            1_742_428_900_000,
        )
        .unwrap();

        let events = issue_events_for(&conn, "issue-aaa").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, IssueEventKind::Create);
        assert_eq!(events[0].author, "scanner:test");
        assert_eq!(events[0].value, None);
        assert_eq!(events[1].kind, IssueEventKind::Close);
        assert_eq!(events[1].author, "user:tester");
        assert_eq!(events[1].value.as_deref(), Some("done in PR 42"));

        let fetched = get(&conn, "issue-aaa").unwrap();
        assert_eq!(fetched.status, IssueStatus::Closed);
    }

    #[test]
    fn reopen_logs_event_and_events_ordered() {
        let conn = open_db_in_memory();
        let issue = sample("issue-aaa", "thinking.empty");
        insert(&conn, &issue).unwrap();

        let created = issue.created;
        close(
            &conn,
            "issue-aaa",
            ClosedReason::Skipped,
            "user:tester",
            None,
            created + 100,
        )
        .unwrap();
        reopen(
            &conn,
            "issue-aaa",
            "user:tester",
            Some("resurfaced"),
            created + 200,
        )
        .unwrap();

        let events = issue_events_for(&conn, "issue-aaa").unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, IssueEventKind::Create);
        assert_eq!(events[1].kind, IssueEventKind::Close);
        assert_eq!(events[1].value, None);
        assert_eq!(events[2].kind, IssueEventKind::Reopen);
        assert_eq!(events[2].value.as_deref(), Some("resurfaced"));

        let fetched = get(&conn, "issue-aaa").unwrap();
        assert_eq!(fetched.status, IssueStatus::Open);
        assert_eq!(fetched.closed_reason, None);
    }

    #[test]
    fn related_notes_ordered_by_timestamp() {
        use crate::note::{NoteValue, insert as insert_note};
        use crate::target::{NoteTarget, SessionTarget};

        let conn = open_db_in_memory();
        let issue = sample("issue-ccc", "thinking.empty");
        insert(&conn, &issue).unwrap();

        for (id, session) in [("note-a", "sess-1"), ("note-b", "sess-2")] {
            insert_note(
                &conn,
                &Note {
                    id: id.to_string(),
                    author: "scanner:test".to_string(),
                    created: 1_742_428_800_000,
                    modified: None,
                    target: NoteTarget::Session(SessionTarget::new(session)),
                    name: "thinking.empty".to_string(),
                    value: NoteValue::from("v"),
                    explanation: None,
                    metadata: None,
                },
            )
            .unwrap();
        }

        // note-b recorded with a lower timestamp so it sorts first
        for (note_id, timestamp) in [("note-a", 200), ("note-b", 100)] {
            insert_issue_evidence(
                &conn,
                &IssueEvidence {
                    issue_id: "issue-ccc".to_string(),
                    note_id: note_id.to_string(),
                    name: "thinking.empty".to_string(),
                    timestamp,
                    digest: None,
                },
            )
            .unwrap();
        }

        let related = related_notes(&conn, "issue-ccc").unwrap();
        assert_eq!(related.len(), 2);
        assert_eq!(related[0].id, "note-b");
        assert_eq!(related[1].id, "note-a");
    }
}
