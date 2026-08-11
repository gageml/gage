use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::note::{Note, target_from_column};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueStatus {
    /// Staged by a model writer; not a docket entry until reconciled.
    Pending,
    Open,
    Closed,
}

impl IssueStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueStatus::Pending => "pending",
            IssueStatus::Open => "open",
            IssueStatus::Closed => "closed",
        }
    }
}

impl std::str::FromStr for IssueStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(IssueStatus::Pending),
            "open" => Ok(IssueStatus::Open),
            "closed" => Ok(IssueStatus::Closed),
            other => Err(format!("unknown issue status '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusReason {
    Completed,
    Skipped,
    /// Closed by reconciliation as a duplicate of a surviving issue.
    Duplicate,
}

impl StatusReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StatusReason::Completed => "completed",
            StatusReason::Skipped => "skipped",
            StatusReason::Duplicate => "duplicate",
        }
    }
}

impl std::str::FromStr for StatusReason {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "completed" => Ok(StatusReason::Completed),
            "skipped" => Ok(StatusReason::Skipped),
            "duplicate" => Ok(StatusReason::Duplicate),
            other => Err(format!("unknown status_reason '{other}'")),
        }
    }
}

/// A change logged against an issue. The variant determines the `type`
/// column; the carried message is the `value` column. `Comment` requires
/// a message; the others carry an optional one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueEvent {
    Create,
    Close { message: Option<String> },
    Reopen { message: Option<String> },
    Promote { message: Option<String> },
    Comment { message: String },
}

impl IssueEvent {
    /// Value of the `type` column for this variant.
    pub fn type_str(&self) -> &'static str {
        match self {
            IssueEvent::Create => "create",
            IssueEvent::Close { .. } => "close",
            IssueEvent::Reopen { .. } => "reopen",
            IssueEvent::Promote { .. } => "promote",
            IssueEvent::Comment { .. } => "comment",
        }
    }

    /// Free-form message carried by the event, stored in the `value`
    /// column. `None` for events without a message.
    pub fn message(&self) -> Option<&str> {
        match self {
            IssueEvent::Create => None,
            IssueEvent::Close { message }
            | IssueEvent::Reopen { message }
            | IssueEvent::Promote { message } => message.as_deref(),
            IssueEvent::Comment { message } => Some(message),
        }
    }

    /// Reconstruct an event from its `type` and `value` columns.
    fn from_columns(type_str: &str, value: Option<String>) -> Result<Self, String> {
        match type_str {
            "create" => Ok(IssueEvent::Create),
            "close" => Ok(IssueEvent::Close { message: value }),
            "reopen" => Ok(IssueEvent::Reopen { message: value }),
            "promote" => Ok(IssueEvent::Promote { message: value }),
            "comment" => Ok(IssueEvent::Comment {
                message: value.unwrap_or_default(),
            }),
            other => Err(format!("unknown issue event type '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    /// Issue name; with `author`, forms the duplication key.
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub status: IssueStatus,
    /// `Some` when `status == Closed`; `None` while `Open`.
    pub status_reason: Option<StatusReason>,
    /// Epoch milliseconds.
    pub created: i64,
    /// Epoch milliseconds. `None` until the issue is updated.
    pub modified: Option<i64>,
    /// Writer identity: `scanner:{name}` for scanner-written issues,
    /// `user:{name}` for issues added by a person, `agent:...` for
    /// model writers (see docs/issues.md). Used to resolve
    /// `scanner:{path}` URIs in issue fields. With `name`, forms the
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

/// A logged change to an issue, recorded in the `issue_event` table:
/// the envelope (who, when, against which issue) wrapping an
/// [`IssueEvent`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggedEvent {
    pub issue_id: String,
    pub author: String,
    /// Epoch milliseconds.
    pub timestamp: i64,
    pub event: IssueEvent,
}

#[derive(Debug)]
pub enum IssueError {
    NotFound(String),
    /// A pending-only transition was applied to a non-pending issue.
    NotPending(String),
    Ambiguous(String, Vec<String>),
    /// An issue with the same `(name, author)` already exists. The
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
            IssueError::NotPending(id) => write!(f, "issue '{id}' is not pending"),
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
                    "duplicate issue (name={}, author={})",
                    prev.name, prev.author
                )
            }
            IssueError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for IssueError {}

impl Issue {
    /// Build a new open issue. The name is used as provided; uniqueness
    /// within the `(name, author)` duplicate key is the caller's concern
    /// (see the author scheme in docs/issues.md).
    pub fn new(name: &str, title: String, description: Option<String>, author: &str) -> Self {
        let id = gage_core::uuid::new_uuid();
        Issue {
            id,
            name: name.to_string(),
            title,
            description,
            status: IssueStatus::Open,
            status_reason: None,
            created: gage_core::datetime::now_ms(),
            modified: None,
            author: author.to_string(),
        }
    }
}

const ISSUE_COLUMNS: &str =
    "id, name, title, description, status, status_reason, created, modified, author";

/// Insert an issue.
///
/// Returns `IssueError::Duplicate(prev)` if an issue with the same
/// `(name, author)` already exists; the existing issue is left
/// untouched and returned so the caller can decide what to do.
pub fn insert(conn: &Connection, issue: &Issue) -> Result<(), IssueError> {
    let tx = conn.unchecked_transaction()?;
    let res = tx.execute(
        &format!(
            "INSERT INTO issue ({ISSUE_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
        ),
        params![
            issue.id,
            issue.name,
            issue.title,
            issue.description,
            issue.status.as_str(),
            issue.status_reason.map(StatusReason::as_str),
            issue.created,
            issue.modified,
            issue.author,
        ],
    );
    if let Err(e) = res {
        if is_unique_violation(&e) {
            let prev = find_by_dup_key(conn, &issue.name, &issue.author)?;
            return Err(IssueError::Duplicate(Box::new(prev)));
        }
        return Err(e.into());
    }
    insert_event(
        &tx,
        &LoggedEvent {
            issue_id: issue.id.clone(),
            author: issue.author.clone(),
            timestamp: issue.created,
            event: IssueEvent::Create,
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

fn find_by_dup_key(conn: &Connection, name: &str, author: &str) -> Result<Issue, IssueError> {
    let mut stmt = conn.prepare(&format!(
        "{ISSUE_SELECT} WHERE i.name = ?1 AND i.author = ?2"
    ))?;
    stmt.query_row(params![name, author], row_to_issue)
        .map_err(IssueError::from)
}

/// Promote a pending issue to open. Bumps `modified` and logs a
/// `Promote` event carrying the optional message. The update and event
/// insert share a transaction. Returns `NotPending` when the issue
/// exists but is not pending.
pub fn promote(
    conn: &Connection,
    issue_id: &str,
    author: &str,
    message: Option<&str>,
    timestamp: i64,
) -> Result<(), IssueError> {
    let tx = conn.unchecked_transaction()?;
    promote_in_tx(&tx, issue_id, author, message, timestamp)?;
    tx.commit()?;
    Ok(())
}

/// [`promote`] without transaction management. The caller holds the
/// enclosing transaction.
pub(crate) fn promote_in_tx(
    tx: &Connection,
    issue_id: &str,
    author: &str,
    message: Option<&str>,
    timestamp: i64,
) -> Result<(), IssueError> {
    let rows = tx.execute(
        "UPDATE issue
         SET status = 'open', modified = ?1
         WHERE id = ?2 AND status = 'pending'",
        params![timestamp, issue_id],
    )?;
    if rows == 0 {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM issue WHERE id = ?1)",
            [issue_id],
            |r| r.get(0),
        )?;
        return Err(if exists {
            IssueError::NotPending(issue_id.to_string())
        } else {
            IssueError::NotFound(issue_id.to_string())
        });
    }
    insert_event(
        tx,
        &LoggedEvent {
            issue_id: issue_id.to_string(),
            author: author.to_string(),
            timestamp,
            event: IssueEvent::Promote {
                message: message.map(str::to_string),
            },
        },
    )?;
    Ok(())
}

/// Reopen a closed issue: clears `status_reason` and sets status back
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
    reopen_in_tx(&tx, issue_id, author, message, timestamp)?;
    tx.commit()?;
    Ok(())
}

/// [`reopen`] without transaction management. The caller holds the
/// enclosing transaction.
pub(crate) fn reopen_in_tx(
    tx: &Connection,
    issue_id: &str,
    author: &str,
    message: Option<&str>,
    timestamp: i64,
) -> Result<(), IssueError> {
    let rows = tx.execute(
        "UPDATE issue
         SET status = 'open', status_reason = NULL, modified = ?1
         WHERE id = ?2",
        params![timestamp, issue_id],
    )?;
    if rows == 0 {
        return Err(IssueError::NotFound(issue_id.to_string()));
    }
    insert_event(
        tx,
        &LoggedEvent {
            issue_id: issue_id.to_string(),
            author: author.to_string(),
            timestamp,
            event: IssueEvent::Reopen {
                message: message.map(str::to_string),
            },
        },
    )?;
    Ok(())
}

/// Mark an issue as closed with the given reason. Idempotent: a
/// repeat close overwrites `status_reason` and bumps `modified`. Logs a
/// `Close` event carrying the optional message. The update and event
/// insert share a transaction.
pub fn close(
    conn: &Connection,
    issue_id: &str,
    reason: StatusReason,
    author: &str,
    message: Option<&str>,
    timestamp: i64,
) -> Result<(), IssueError> {
    let tx = conn.unchecked_transaction()?;
    close_in_tx(&tx, issue_id, reason, author, message, timestamp)?;
    tx.commit()?;
    Ok(())
}

/// [`close`] without transaction management. The caller holds the
/// enclosing transaction.
pub(crate) fn close_in_tx(
    tx: &Connection,
    issue_id: &str,
    reason: StatusReason,
    author: &str,
    message: Option<&str>,
    timestamp: i64,
) -> Result<(), IssueError> {
    let rows = tx.execute(
        "UPDATE issue
         SET status = 'closed', status_reason = ?1, modified = ?2
         WHERE id = ?3",
        params![reason.as_str(), timestamp, issue_id],
    )?;
    if rows == 0 {
        return Err(IssueError::NotFound(issue_id.to_string()));
    }
    insert_event(
        tx,
        &LoggedEvent {
            issue_id: issue_id.to_string(),
            author: author.to_string(),
            timestamp,
            event: IssueEvent::Close {
                message: message.map(str::to_string),
            },
        },
    )?;
    Ok(())
}

/// Record a comment against an issue. Bumps `modified` so it reflects
/// last activity, and logs a `Comment` event carrying the message. The
/// update and event insert share a transaction.
pub fn comment(
    conn: &Connection,
    issue_id: &str,
    author: &str,
    message: &str,
    timestamp: i64,
) -> Result<(), IssueError> {
    let tx = conn.unchecked_transaction()?;
    comment_in_tx(&tx, issue_id, author, message, timestamp)?;
    tx.commit()?;
    Ok(())
}

/// [`comment`] without transaction management. The caller holds the
/// enclosing transaction.
pub(crate) fn comment_in_tx(
    tx: &Connection,
    issue_id: &str,
    author: &str,
    message: &str,
    timestamp: i64,
) -> Result<(), IssueError> {
    let rows = tx.execute(
        "UPDATE issue SET modified = ?1 WHERE id = ?2",
        params![timestamp, issue_id],
    )?;
    if rows == 0 {
        return Err(IssueError::NotFound(issue_id.to_string()));
    }
    insert_event(
        tx,
        &LoggedEvent {
            issue_id: issue_id.to_string(),
            author: author.to_string(),
            timestamp,
            event: IssueEvent::Comment {
                message: message.to_string(),
            },
        },
    )?;
    Ok(())
}

/// Append an issue event.
pub fn insert_issue_event(conn: &Connection, event: &LoggedEvent) -> Result<(), IssueError> {
    insert_event(conn, event)
}

fn insert_event(conn: &Connection, event: &LoggedEvent) -> Result<(), IssueError> {
    conn.execute(
        "INSERT INTO issue_event (issue_id, type, author, timestamp, value)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            event.issue_id,
            event.event.type_str(),
            event.author,
            event.timestamp,
            event.event.message(),
        ],
    )?;
    Ok(())
}

/// Events logged against `issue_id`, ordered by `timestamp` ascending.
pub fn issue_events_for(conn: &Connection, issue_id: &str) -> Result<Vec<LoggedEvent>, IssueError> {
    let mut stmt = conn.prepare(
        "SELECT issue_id, type, author, timestamp, value
         FROM issue_event WHERE issue_id = ?1 ORDER BY timestamp ASC",
    )?;
    let rows = stmt
        .query_map([issue_id], row_to_event)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn row_to_event(row: &rusqlite::Row) -> rusqlite::Result<LoggedEvent> {
    let type_str: String = row.get(1)?;
    let value: Option<String> = row.get(4)?;
    let event = IssueEvent::from_columns(&type_str, value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(e)),
        )
    })?;
    Ok(LoggedEvent {
        issue_id: row.get(0)?,
        author: row.get(2)?,
        timestamp: row.get(3)?,
        event,
    })
}

/// Delete an issue along with its `issue_evidence` links, `issue_event`
/// log, and `scan_issue` links. Evidence notes and scans are not deleted —
/// only the link rows go. Notes target sessions/scans/projects, never an
/// issue, so the issue owns no notes of its own.
pub fn delete(conn: &Connection, issue_id: &str) -> Result<(), IssueError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM issue_evidence WHERE issue_id = ?1", [issue_id])?;
    tx.execute("DELETE FROM session_issue WHERE issue_id = ?1", [issue_id])?;
    tx.execute("DELETE FROM issue_event WHERE issue_id = ?1", [issue_id])?;
    tx.execute("DELETE FROM scan_issue WHERE issue_id = ?1", [issue_id])?;
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

/// Links `issue_id` to `session_id`. Returns true when the link is new,
/// false when it was already recorded.
pub fn insert_session_issue(
    conn: &Connection,
    session_id: &str,
    issue_id: &str,
) -> Result<bool, IssueError> {
    let rows = conn.execute(
        "INSERT OR IGNORE INTO session_issue (session_id, issue_id) VALUES (?1, ?2)",
        params![session_id, issue_id],
    )?;
    Ok(rows > 0)
}

/// Session IDs linked to `issue_id` via `session_issue`.
pub fn issue_sessions(conn: &Connection, issue_id: &str) -> Result<Vec<String>, IssueError> {
    let mut stmt = conn
        .prepare("SELECT session_id FROM session_issue WHERE issue_id = ?1 ORDER BY session_id")?;
    let rows = stmt
        .query_map([issue_id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
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
    /// Only `pending` issues.
    Pending,
    /// Open and pending issues; excludes closed.
    Unresolved,
    /// Open and closed issues; excludes pending.
    Reconciled,
    /// All issues regardless of status.
    Any,
}

#[derive(Debug, Default)]
pub struct IssueFilters {
    pub status: IssueStatusFilter,
    pub name: Option<String>,
}

const ISSUE_SELECT: &str = "SELECT i.id, i.name, i.title, i.description, i.status,
            i.status_reason, i.created, i.modified, i.author
     FROM issue i";

pub fn find(conn: &Connection, filters: &IssueFilters) -> Result<Vec<Issue>, IssueError> {
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    match filters.status {
        IssueStatusFilter::Open => clauses.push("i.status = 'open'".to_string()),
        IssueStatusFilter::Closed => clauses.push("i.status = 'closed'".to_string()),
        IssueStatusFilter::Pending => clauses.push("i.status = 'pending'".to_string()),
        IssueStatusFilter::Unresolved => {
            clauses.push("i.status IN ('open', 'pending')".to_string())
        }
        IssueStatusFilter::Reconciled => clauses.push("i.status IN ('open', 'closed')".to_string()),
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
    let status_str: String = row.get(4)?;
    let status = status_str.parse::<IssueStatus>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(e)),
        )
    })?;
    let status_reason: Option<String> = row.get(5)?;
    let status_reason = status_reason
        .map(|s| {
            s.parse::<StatusReason>().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other(e)),
                )
            })
        })
        .transpose()?;
    Ok(Issue {
        id: row.get(0)?,
        name: row.get(1)?,
        title: row.get(2)?,
        description: row.get(3)?,
        status,
        status_reason,
        created: row.get(6)?,
        modified: row.get(7)?,
        author: row.get(8)?,
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
            title: "Sample title".to_string(),
            description: Some("scanner:description.md".to_string()),
            status: IssueStatus::Open,
            status_reason: None,
            created: 1_742_428_800_000,
            modified: None,
            author: "scanner:test".to_string(),
        }
    }

    #[test]
    fn insert_and_get() {
        let conn = open_db_in_memory().unwrap();
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
        let conn = open_db_in_memory().unwrap();
        let i1 = sample("issue-aaa", "n1");
        let mut i2 = sample("issue-bbb", "n2");
        i2.status = IssueStatus::Closed;
        i2.status_reason = Some(StatusReason::Completed);
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
        assert_eq!(closed[0].status_reason, Some(StatusReason::Completed));
    }

    #[test]
    fn duplicate_key_returns_prev_and_keeps_one_row() {
        let conn = open_db_in_memory().unwrap();
        let a = sample("issue-aaa", "thinking.empty");
        insert(&conn, &a).unwrap();

        // Same name, fresh id and different title
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
    fn insert_issue_evidence_link() {
        use crate::note::{NoteValue, insert as insert_note};
        use crate::target::{NoteTarget, SessionTarget};

        let conn = open_db_in_memory().unwrap();
        let issue = sample("issue-bbb", "thinking.empty");
        insert(&conn, &issue).unwrap();

        let note = Note {
            id: "note-1".to_string(),
            author: "user:test".to_string(),
            created: 1_742_428_800_000,
            modified: None,
            target: NoteTarget::Session(SessionTarget::new("11111111-1111-1111-1111-111111111111")),
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
    fn promote_pending_issue_opens_and_logs_event() {
        let conn = open_db_in_memory().unwrap();
        let mut issue = sample("issue-aaa", "thinking.empty");
        issue.status = IssueStatus::Pending;
        insert(&conn, &issue).unwrap();

        promote(
            &conn,
            "issue-aaa",
            "scanner:reconcile",
            Some("novel"),
            issue.created + 100,
        )
        .unwrap();

        let fetched = get(&conn, "issue-aaa").unwrap();
        assert_eq!(fetched.status, IssueStatus::Open);
        assert_eq!(fetched.modified, Some(issue.created + 100));

        let events = issue_events_for(&conn, "issue-aaa").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1].event,
            IssueEvent::Promote {
                message: Some("novel".to_string())
            }
        );
    }

    #[test]
    fn promote_non_pending_issue_errors() {
        let conn = open_db_in_memory().unwrap();
        let issue = sample("issue-aaa", "thinking.empty");
        insert(&conn, &issue).unwrap();

        assert!(matches!(
            promote(&conn, "issue-aaa", "scanner:reconcile", None, 1),
            Err(IssueError::NotPending(_))
        ));
        assert!(matches!(
            promote(&conn, "nope", "scanner:reconcile", None, 1),
            Err(IssueError::NotFound(_))
        ));
    }

    #[test]
    fn pending_issues_filtered_from_open_and_reconciled() {
        let conn = open_db_in_memory().unwrap();
        let open_issue = sample("issue-aaa", "n1");
        let mut pending_issue = sample("issue-bbb", "n2");
        pending_issue.status = IssueStatus::Pending;
        insert(&conn, &open_issue).unwrap();
        insert(&conn, &pending_issue).unwrap();

        let open = find(&conn, &IssueFilters::default()).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "issue-aaa");

        let pending = find(
            &conn,
            &IssueFilters {
                status: IssueStatusFilter::Pending,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "issue-bbb");

        let reconciled = find(
            &conn,
            &IssueFilters {
                status: IssueStatusFilter::Reconciled,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].id, "issue-aaa");

        let unresolved = find(
            &conn,
            &IssueFilters {
                status: IssueStatusFilter::Unresolved,
                ..Default::default()
            },
        )
        .unwrap();
        // Both samples share `created`, so relative order is unspecified
        let mut ids: Vec<&str> = unresolved.iter().map(|i| i.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["issue-aaa", "issue-bbb"]);

        let all = find(
            &conn,
            &IssueFilters {
                status: IssueStatusFilter::Any,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn close_logs_event_with_message() {
        let conn = open_db_in_memory().unwrap();
        let issue = sample("issue-aaa", "thinking.empty");
        insert(&conn, &issue).unwrap();

        close(
            &conn,
            "issue-aaa",
            StatusReason::Completed,
            "user:tester",
            Some("done in PR 42"),
            1_742_428_900_000,
        )
        .unwrap();

        let events = issue_events_for(&conn, "issue-aaa").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, IssueEvent::Create);
        assert_eq!(events[0].author, "scanner:test");
        assert_eq!(events[1].author, "user:tester");
        assert_eq!(
            events[1].event,
            IssueEvent::Close {
                message: Some("done in PR 42".to_string())
            }
        );

        let fetched = get(&conn, "issue-aaa").unwrap();
        assert_eq!(fetched.status, IssueStatus::Closed);
    }

    #[test]
    fn reopen_logs_event_and_events_ordered() {
        let conn = open_db_in_memory().unwrap();
        let issue = sample("issue-aaa", "thinking.empty");
        insert(&conn, &issue).unwrap();

        let created = issue.created;
        close(
            &conn,
            "issue-aaa",
            StatusReason::Skipped,
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
        assert_eq!(events[0].event, IssueEvent::Create);
        assert_eq!(events[1].event, IssueEvent::Close { message: None });
        assert_eq!(
            events[2].event,
            IssueEvent::Reopen {
                message: Some("resurfaced".to_string())
            }
        );

        let fetched = get(&conn, "issue-aaa").unwrap();
        assert_eq!(fetched.status, IssueStatus::Open);
        assert_eq!(fetched.status_reason, None);
    }

    #[test]
    fn comment_logs_event_and_bumps_modified() {
        let conn = open_db_in_memory().unwrap();
        let issue = sample("issue-aaa", "thinking.empty");
        insert(&conn, &issue).unwrap();

        comment(
            &conn,
            "issue-aaa",
            "user:tester",
            "looks related to PR 42",
            issue.created + 100,
        )
        .unwrap();

        let events = issue_events_for(&conn, "issue-aaa").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, IssueEvent::Create);
        assert_eq!(events[1].author, "user:tester");
        assert_eq!(
            events[1].event,
            IssueEvent::Comment {
                message: "looks related to PR 42".to_string()
            }
        );

        let fetched = get(&conn, "issue-aaa").unwrap();
        assert_eq!(fetched.modified, Some(issue.created + 100));
        assert_eq!(fetched.status, IssueStatus::Open);
    }

    #[test]
    fn comment_unknown_issue_is_not_found() {
        let conn = open_db_in_memory().unwrap();
        assert!(matches!(
            comment(&conn, "nope", "user:tester", "hi", 1),
            Err(IssueError::NotFound(_))
        ));
    }

    #[test]
    fn delete_removes_event_log() {
        let conn = open_db_in_memory().unwrap();
        let issue = sample("issue-aaa", "thinking.empty");
        insert(&conn, &issue).unwrap();
        close(
            &conn,
            "issue-aaa",
            StatusReason::Completed,
            "user:tester",
            Some("done"),
            issue.created + 100,
        )
        .unwrap();

        delete(&conn, "issue-aaa").unwrap();

        assert!(matches!(
            get(&conn, "issue-aaa"),
            Err(IssueError::NotFound(_))
        ));
        let count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM issue_event WHERE issue_id = ?1",
                ["issue-aaa"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn related_notes_ordered_by_timestamp() {
        use crate::note::{NoteValue, insert as insert_note};
        use crate::target::{NoteTarget, SessionTarget};

        let conn = open_db_in_memory().unwrap();
        let issue = sample("issue-ccc", "thinking.empty");
        insert(&conn, &issue).unwrap();

        for (id, session) in [
            ("note-a", "11111111-1111-1111-1111-111111111111"),
            ("note-b", "22222222-2222-2222-2222-222222222222"),
        ] {
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
