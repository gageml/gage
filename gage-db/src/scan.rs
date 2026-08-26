use std::collections::HashMap;

use rusqlite::{Connection, params};

#[derive(Debug, Clone)]
pub struct Scan {
    pub id: String,
    /// Epoch milliseconds.
    pub created: i64,
    pub metadata: Option<String>,
}

/// One planned task invocation within a scan. Inserted when the plan
/// is built (`pending`), updated on dispatch (`started`) and again on
/// completion. A non-terminal status is the last known state: a scan
/// that died leaves its rows at `pending`/`started`.
#[derive(Debug, Clone)]
pub struct ScanTask {
    pub scan_id: String,
    pub scanner_name: String,
    pub scanner_version: String,
    pub task_name: String,
    pub status: TaskStatus,
    /// Epoch milliseconds; None when the task never ran.
    pub started: Option<i64>,
    /// Epoch milliseconds; None when the task never finished.
    pub stopped: Option<i64>,
    pub error: Option<String>,
}

/// One `call_agent` invocation made by a task. Inserted when the
/// claude process reports its session id, finalized when the process
/// exits. `result` is claude's terminal `result` message verbatim;
/// NULL with a non-NULL `exit_code` marks an abnormal termination.
#[derive(Debug, Clone)]
pub struct TaskAgent {
    pub session_id: String,
    pub scan_id: String,
    pub scanner_name: String,
    pub task_name: String,
    pub exit_code: Option<i64>,
    pub stderr: Option<String>,
    pub result: Option<String>,
}

#[derive(Debug)]
pub enum ScanError {
    NotFound(String),
    Ambiguous(String, Vec<String>),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for ScanError {
    fn from(e: rusqlite::Error) -> Self {
        ScanError::Db(e)
    }
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::NotFound(id) => write!(f, "scan '{id}' not found"),
            ScanError::Ambiguous(prefix, ids) => {
                write!(f, "Found more than one scan matching {prefix}")?;
                for id in ids {
                    write!(f, "\n  {id}")?;
                }
                Ok(())
            }
            ScanError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for ScanError {}

pub fn insert_scan(conn: &Connection, scan: &Scan) -> Result<(), ScanError> {
    conn.execute(
        "INSERT INTO scan (id, created, metadata) VALUES (?1, ?2, ?3)",
        params![scan.id, scan.created, scan.metadata],
    )?;
    Ok(())
}

pub fn insert_task(conn: &Connection, task: &ScanTask) -> Result<(), ScanError> {
    conn.execute(
        "INSERT INTO scan_task (scan_id, scanner_name, scanner_version, task_name, \
                                status, started, stopped, error) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            task.scan_id,
            task.scanner_name,
            task.scanner_version,
            task.task_name,
            task.status.as_str(),
            task.started,
            task.stopped,
            task.error,
        ],
    )?;
    Ok(())
}

/// Mark a task dispatched.
pub fn start_task(
    conn: &Connection,
    scan_id: &str,
    scanner_name: &str,
    task_name: &str,
    started_ms: i64,
) -> Result<(), ScanError> {
    conn.execute(
        "UPDATE scan_task SET status = ?4, started = ?5 \
         WHERE scan_id = ?1 AND scanner_name = ?2 AND task_name = ?3",
        params![
            scan_id,
            scanner_name,
            task_name,
            TaskStatus::Started.as_str(),
            started_ms,
        ],
    )?;
    Ok(())
}

/// Record a task's terminal outcome. A skipped task never ran, so its
/// `started` is cleared along with `stopped` staying NULL.
pub fn finish_task(
    conn: &Connection,
    scan_id: &str,
    scanner_name: &str,
    task_name: &str,
    status: TaskStatus,
    stopped_ms: Option<i64>,
    error: Option<&str>,
) -> Result<(), ScanError> {
    if status == TaskStatus::Skipped {
        conn.execute(
            "UPDATE scan_task SET status = ?4, started = NULL, stopped = NULL, error = NULL \
             WHERE scan_id = ?1 AND scanner_name = ?2 AND task_name = ?3",
            params![scan_id, scanner_name, task_name, status.as_str()],
        )?;
    } else {
        conn.execute(
            "UPDATE scan_task SET status = ?4, stopped = ?5, error = ?6 \
             WHERE scan_id = ?1 AND scanner_name = ?2 AND task_name = ?3",
            params![
                scan_id,
                scanner_name,
                task_name,
                status.as_str(),
                stopped_ms,
                error,
            ],
        )?;
    }
    Ok(())
}

/// Finalize a canceled scan's unfinished tasks: every `started` row is
/// marked `canceled` with the given stop time, and every `pending` row
/// (never dispatched) is marked `canceled` with no stop time. Returns
/// the number of rows updated.
pub fn cancel_unfinished_tasks(
    conn: &Connection,
    scan_id: &str,
    stopped_ms: i64,
) -> Result<usize, ScanError> {
    let started = conn.execute(
        "UPDATE scan_task SET status = ?2, stopped = ?3 \
         WHERE scan_id = ?1 AND status = ?4",
        params![
            scan_id,
            TaskStatus::Canceled.as_str(),
            stopped_ms,
            TaskStatus::Started.as_str(),
        ],
    )?;
    let pending = conn.execute(
        "UPDATE scan_task SET status = ?2 WHERE scan_id = ?1 AND status = ?3",
        params![
            scan_id,
            TaskStatus::Canceled.as_str(),
            TaskStatus::Pending.as_str(),
        ],
    )?;
    Ok(started + pending)
}

/// Record an agent session spawned by a task. Called when the claude
/// process reports its session id; `exit_code`/`stderr`/`result` are
/// filled in by [`finish_task_agent`] when the process exits.
pub fn insert_task_agent(conn: &Connection, agent: &TaskAgent) -> Result<(), ScanError> {
    conn.execute(
        "INSERT INTO task_agent (session_id, scan_id, scanner_name, task_name, \
                                 exit_code, stderr, result) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            agent.session_id,
            agent.scan_id,
            agent.scanner_name,
            agent.task_name,
            agent.exit_code,
            agent.stderr,
            agent.result,
        ],
    )?;
    Ok(())
}

pub fn finish_task_agent(
    conn: &Connection,
    session_id: &str,
    exit_code: i64,
    stderr: &str,
    result: Option<&str>,
) -> Result<(), ScanError> {
    conn.execute(
        "UPDATE task_agent SET exit_code = ?2, stderr = ?3, result = ?4 WHERE session_id = ?1",
        params![session_id, exit_code, stderr, result],
    )?;
    Ok(())
}

/// Payload persisted to `scan.metadata`. A run writes [`RunningScan`]
/// at start and replaces it at wind-down: a scan writes its task
/// summary, an agent run its agent-run information. A `Running`
/// payload whose pid is dead marks a run that died; absent metadata
/// (NULL column) is the same last-known state for runs predating the
/// running marker.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum ScanMetadata {
    Scan(ScanSummary),
    Agent(AgentRunSummary),
    Running(RunningScan),
}

impl ScanMetadata {
    /// Run duration; None while the run is still marked running.
    pub fn elapsed_ms(&self) -> Option<u64> {
        match self {
            ScanMetadata::Scan(s) => Some(s.elapsed_ms),
            ScanMetadata::Agent(a) => Some(a.elapsed_ms),
            ScanMetadata::Running(_) => None,
        }
    }
}

/// Startup payload for `scan.metadata`: the process running the scan.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunningScan {
    pub pid: u32,
}

/// `scan.metadata` startup payload marking the scan as running.
pub fn running_metadata(pid: u32) -> String {
    serde_json::to_string(&ScanMetadata::Running(RunningScan { pid })).unwrap()
}

impl Scan {
    /// Parse `metadata` into its payload. None when the column is
    /// NULL — a run predating the running marker that never completed.
    pub fn parse_metadata(&self) -> Result<Option<ScanMetadata>, serde_json::Error> {
        self.metadata
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
    }
}

/// End-of-run summary for a scan run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScanSummary {
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub skipped: usize,
    /// The run was canceled by the user before completing. Defaults to
    /// false so summaries written before the field existed parse.
    #[serde(default)]
    pub canceled: bool,
    pub elapsed_ms: u64,
}

/// Task state as recorded in `scan_task.status`. `Pending` and
/// `Started` are last-known states, not liveness claims: a scan that
/// died leaves its rows there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Started,
    Completed,
    Failed,
    Skipped,
    Canceled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Started => "started",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Skipped => "skipped",
            TaskStatus::Canceled => "canceled",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => TaskStatus::Pending,
            "started" => TaskStatus::Started,
            "completed" => TaskStatus::Completed,
            "failed" => TaskStatus::Failed,
            "skipped" => TaskStatus::Skipped,
            "canceled" => TaskStatus::Canceled,
            _ => return None,
        })
    }
}

/// End-of-run information for an agent run's proxy scan.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentRunSummary {
    /// Agent reference as `<scanner>::<fn>`.
    pub agent: String,
    pub elapsed_ms: u64,
    pub is_error: bool,
}

pub fn set_agent_summary(
    conn: &Connection,
    scan_id: &str,
    summary: &AgentRunSummary,
) -> Result<(), ScanError> {
    let json = serde_json::to_string(summary).unwrap();
    conn.execute(
        "UPDATE scan SET metadata = ?2 WHERE id = ?1",
        params![scan_id, json],
    )?;
    Ok(())
}

pub fn set_scan_summary(
    conn: &Connection,
    scan_id: &str,
    summary: &ScanSummary,
) -> Result<(), ScanError> {
    let json = serde_json::to_string(summary).unwrap();
    conn.execute(
        "UPDATE scan SET metadata = ?2 WHERE id = ?1",
        params![scan_id, json],
    )?;
    Ok(())
}

/// Record that `session_id` was selected for `scan_id`. `agent_corpus`
/// marks a session living in the agent corpus (`<gage_home>/claude`)
/// rather than the default Claude projects corpus; the location is
/// encoded into `scan_session.metadata` here and decoded by
/// [`scan_session_rows`].
pub fn insert_scan_session(
    conn: &Connection,
    scan_id: &str,
    session_id: &str,
    agent_corpus: bool,
) -> Result<(), ScanError> {
    let metadata = agent_corpus.then_some(AGENT_SESSION_METADATA);
    conn.execute(
        "INSERT INTO scan_session (scan_id, session_id, metadata) VALUES (?1, ?2, ?3)",
        params![scan_id, session_id, metadata],
    )?;
    Ok(())
}

/// One `scan_session` row: a session selected for a scan, with where
/// the session lives decoded from the row's metadata.
#[derive(Debug, Clone)]
pub struct ScanSessionRow {
    pub session_id: String,
    /// The session lives in the agent corpus
    pub agent: bool,
}

/// The sessions selected for `scan_id`, ordered by session id.
pub fn scan_session_rows(
    conn: &Connection,
    scan_id: &str,
) -> Result<Vec<ScanSessionRow>, ScanError> {
    let mut stmt = conn.prepare(
        "SELECT session_id, metadata FROM scan_session WHERE scan_id = ?1 ORDER BY session_id",
    )?;
    let rows = stmt
        .query_map(params![scan_id], |row| {
            let metadata: Option<String> = row.get(1)?;
            Ok(ScanSessionRow {
                session_id: row.get(0)?,
                agent: session_metadata_is_agent(metadata.as_deref()),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `scan_session.metadata` payload for an agent-corpus session. Absent
/// metadata means the default Claude projects corpus.
const AGENT_SESSION_METADATA: &str = r#"{"corpus":"agent"}"#;

/// Unparseable or unrecognized metadata decodes as the default corpus —
/// the same fallback as absent metadata, surfacing as a session lookup
/// miss rather than an error.
fn session_metadata_is_agent(metadata: Option<&str>) -> bool {
    metadata
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .and_then(|v| v.get("corpus").and_then(|c| c.as_str().map(String::from)))
        .is_some_and(|c| c == "agent")
}

/// How a scan is linked to a note: the scan wrote the note's value, or
/// it carried a prior note forward into its visible set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanNoteRole {
    Wrote,
    Carried,
}

impl ScanNoteRole {
    fn as_str(self) -> &'static str {
        match self {
            ScanNoteRole::Wrote => "wrote",
            ScanNoteRole::Carried => "carried",
        }
    }
}

/// Record that `note_id` was written during (or carried forward into)
/// `scan_id`. A repeat link in the same scan is a no-op, except that a
/// `Wrote` link upgrades an existing `Carried` link — a write always
/// wins attribution, and a carry never downgrades it.
pub fn insert_scan_note(
    conn: &Connection,
    scan_id: &str,
    note_id: &str,
    role: ScanNoteRole,
) -> Result<(), ScanError> {
    conn.execute(
        "INSERT INTO scan_note (scan_id, note_id, role) VALUES (?1, ?2, ?3)
         ON CONFLICT (scan_id, note_id) DO UPDATE SET role = excluded.role
         WHERE excluded.role = 'wrote'",
        params![scan_id, note_id, role.as_str()],
    )?;
    Ok(())
}

/// Record that `issue_id` was written during `scan_id`. A repeat write of
/// the same issue in the same scan is a no-op.
pub fn insert_scan_issue(
    conn: &Connection,
    scan_id: &str,
    issue_id: &str,
) -> Result<(), ScanError> {
    conn.execute(
        "INSERT OR IGNORE INTO scan_issue (scan_id, issue_id) VALUES (?1, ?2)",
        params![scan_id, issue_id],
    )?;
    Ok(())
}

pub fn session_ids_for_scan(conn: &Connection, scan_id: &str) -> Result<Vec<String>, ScanError> {
    let mut stmt =
        conn.prepare("SELECT session_id FROM scan_session WHERE scan_id = ?1 ORDER BY session_id")?;
    let ids = stmt
        .query_map(params![scan_id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Session ids of the agent runs a scan's tasks spawned, ordered by
/// session id.
pub fn agent_session_ids_for_scan(
    conn: &Connection,
    scan_id: &str,
) -> Result<Vec<String>, ScanError> {
    let mut stmt =
        conn.prepare("SELECT session_id FROM task_agent WHERE scan_id = ?1 ORDER BY session_id")?;
    let ids = stmt
        .query_map(params![scan_id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

pub fn note_ids_for_scan(conn: &Connection, scan_id: &str) -> Result<Vec<String>, ScanError> {
    let mut stmt = conn.prepare("SELECT note_id FROM scan_note WHERE scan_id = ?1")?;
    let ids = stmt
        .query_map(params![scan_id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

pub fn issue_ids_for_scan(conn: &Connection, scan_id: &str) -> Result<Vec<String>, ScanError> {
    let mut stmt = conn.prepare("SELECT issue_id FROM scan_issue WHERE scan_id = ?1")?;
    let ids = stmt
        .query_map(params![scan_id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Per-scan row counts for list displays, keyed by scan id. Scans
/// with no task or session rows are absent from the map.
pub fn counts_by_scan(conn: &Connection) -> Result<HashMap<String, ScanCounts>, ScanError> {
    let mut out: HashMap<String, ScanCounts> = HashMap::new();
    count_by_scan(conn, "scan_task", &mut out, |c, n| c.tasks = n)?;
    count_by_scan(conn, "scan_session", &mut out, |c, n| c.sessions = n)?;
    count_by_scan(conn, "scan_note", &mut out, |c, n| c.notes = n)?;
    count_by_scan(conn, "scan_issue", &mut out, |c, n| c.issues = n)?;
    Ok(out)
}

/// Fold `table`'s per-scan row counts into `out` via `set`.
fn count_by_scan(
    conn: &Connection,
    table: &str,
    out: &mut HashMap<String, ScanCounts>,
    set: impl Fn(&mut ScanCounts, u32),
) -> Result<(), ScanError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT scan_id, COUNT(*) FROM {table} GROUP BY scan_id"
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
    })?;
    for row in rows {
        let (id, n) = row?;
        set(out.entry(id).or_default(), n);
    }
    Ok(())
}

/// Tasks recorded, sessions touched, and notes/issues linked by one scan.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanCounts {
    pub tasks: u32,
    pub sessions: u32,
    pub notes: u32,
    pub issues: u32,
}

pub fn all(conn: &Connection) -> Result<Vec<Scan>, ScanError> {
    let mut stmt = conn.prepare("SELECT id, created, metadata FROM scan ORDER BY created DESC")?;
    let scans = stmt
        .query_map([], |row| {
            Ok(Scan {
                id: row.get(0)?,
                created: row.get(1)?,
                metadata: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(scans)
}

/// Tasks recorded for a scan, in (scanner_name, task_name) order.
pub fn tasks_for_scan(conn: &Connection, scan_id: &str) -> Result<Vec<ScanTask>, ScanError> {
    let mut stmt = conn.prepare(
        "SELECT scan_id, scanner_name, scanner_version, task_name, status, started, stopped, error \
         FROM scan_task WHERE scan_id = ?1 ORDER BY scanner_name, task_name",
    )?;
    let tasks = stmt
        .query_map(params![scan_id], |row| {
            let status: String = row.get(4)?;
            Ok(ScanTask {
                scan_id: row.get(0)?,
                scanner_name: row.get(1)?,
                scanner_version: row.get(2)?,
                task_name: row.get(3)?,
                status: TaskStatus::from_str(&status).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        format!("unknown task status '{status}'").into(),
                    )
                })?,
                started: row.get(5)?,
                stopped: row.get(6)?,
                error: row.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(tasks)
}

/// Recorded agent spend for a scan. `total_usd` sums each
/// `task_agent` result's `total_cost_usd`. `incomplete` is true when
/// any agent has no recorded cost (abnormal termination or still
/// running), so the total understates true spend.
#[derive(Debug, Clone, Copy)]
pub struct ScanCost {
    pub total_usd: f64,
    pub incomplete: bool,
}

pub fn cost_for_scan(conn: &Connection, scan_id: &str) -> Result<ScanCost, ScanError> {
    let cost = conn.query_row(
        "SELECT COALESCE(SUM(json_extract(result, '$.total_cost_usd')), 0.0), \
                COUNT(*) FILTER (WHERE json_extract(result, '$.total_cost_usd') IS NULL) > 0 \
         FROM task_agent WHERE scan_id = ?1",
        params![scan_id],
        |row| {
            Ok(ScanCost {
                total_usd: row.get(0)?,
                incomplete: row.get(1)?,
            })
        },
    )?;
    Ok(cost)
}

/// A task's recorded agent spend, keyed by scanner and task name.
#[derive(Debug, Clone)]
pub struct TaskCost {
    pub scanner_name: String,
    pub task_name: String,
    pub cost: ScanCost,
}

/// Per-task agent spend for a scan. Tasks with no `task_agent` rows
/// contribute no entry.
pub fn costs_for_tasks(conn: &Connection, scan_id: &str) -> Result<Vec<TaskCost>, ScanError> {
    let mut stmt = conn.prepare(
        "SELECT scanner_name, task_name, \
                COALESCE(SUM(json_extract(result, '$.total_cost_usd')), 0.0), \
                COUNT(*) FILTER (WHERE json_extract(result, '$.total_cost_usd') IS NULL) > 0 \
         FROM task_agent WHERE scan_id = ?1 \
         GROUP BY scanner_name, task_name",
    )?;
    let costs = stmt
        .query_map(params![scan_id], |row| {
            Ok(TaskCost {
                scanner_name: row.get(0)?,
                task_name: row.get(1)?,
                cost: ScanCost {
                    total_usd: row.get(2)?,
                    incomplete: row.get(3)?,
                },
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(costs)
}

/// Agent sessions recorded for a scan, in (scanner_name, task_name,
/// session_id) order.
pub fn agents_for_scan(conn: &Connection, scan_id: &str) -> Result<Vec<TaskAgent>, ScanError> {
    let mut stmt = conn.prepare(
        "SELECT session_id, scan_id, scanner_name, task_name, exit_code, stderr, result \
         FROM task_agent WHERE scan_id = ?1 \
         ORDER BY scanner_name, task_name, session_id",
    )?;
    let agents = stmt
        .query_map(params![scan_id], |row| {
            Ok(TaskAgent {
                session_id: row.get(0)?,
                scan_id: row.get(1)?,
                scanner_name: row.get(2)?,
                task_name: row.get(3)?,
                exit_code: row.get(4)?,
                stderr: row.get(5)?,
                result: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(agents)
}

/// Distinct scanner names recorded for a scan, ascending.
pub fn scanner_names_for_scan(conn: &Connection, scan_id: &str) -> Result<Vec<String>, ScanError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT scanner_name FROM scan_task WHERE scan_id = ?1 ORDER BY scanner_name",
    )?;
    let names = stmt
        .query_map(params![scan_id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names)
}

/// Delete a scan's run metadata: the `scan` row plus its `scan_task`,
/// `task_agent`, and `scan_session` rows. Notes and issues are never
/// deleted here — note targets are advisory references with no
/// lifetime coupling (see [`NoteTarget`](crate::target::NoteTarget)), so even a note targeting
/// this scan outlives it by design.
pub fn delete_scan(conn: &Connection, scan_id: &str) -> Result<(), ScanError> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM scan WHERE id = ?1",
        params![scan_id],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(ScanError::NotFound(scan_id.to_string()));
    }

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM task_agent WHERE scan_id = ?1",
        params![scan_id],
    )?;
    tx.execute("DELETE FROM scan_task WHERE scan_id = ?1", params![scan_id])?;
    tx.execute(
        "DELETE FROM scan_session WHERE scan_id = ?1",
        params![scan_id],
    )?;
    tx.execute("DELETE FROM scan_note WHERE scan_id = ?1", params![scan_id])?;
    tx.execute(
        "DELETE FROM scan_issue WHERE scan_id = ?1",
        params![scan_id],
    )?;
    tx.execute("DELETE FROM scan WHERE id = ?1", params![scan_id])?;
    tx.commit()?;

    Ok(())
}

/// Every scan id, unordered. Peer set for prefix-disambiguated
/// displays; matches what [`get_scan`] resolves against.
pub fn all_ids(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM scan")?;
    stmt.query_map([], |row| row.get::<_, String>(0))?.collect()
}

/// Look up a scan by ID prefix.
pub fn get_scan(conn: &Connection, id_prefix: &str) -> Result<Scan, ScanError> {
    let pattern = format!("{id_prefix}%");
    let mut stmt = conn.prepare("SELECT id, created, metadata FROM scan WHERE id LIKE ?1")?;
    let scans: Vec<Scan> = stmt
        .query_map([&pattern], |row| {
            Ok(Scan {
                id: row.get(0)?,
                created: row.get(1)?,
                metadata: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    match scans.len() {
        0 => Err(ScanError::NotFound(id_prefix.to_string())),
        1 => Ok(scans.into_iter().next().unwrap()),
        _ => {
            let mut ids: Vec<String> = scans.into_iter().map(|s| s.id).collect();
            ids.sort();
            Err(ScanError::Ambiguous(id_prefix.to_string(), ids))
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unused_result_ok)]
mod tests {
    use super::*;
    use crate::db::open_db_in_memory;
    use crate::note::{self, Note, NoteValue};
    use crate::target::{NoteTarget, SessionTarget};

    fn test_scan() -> Scan {
        Scan {
            id: "scan-001".to_string(),
            created: 1_743_984_000_000,
            metadata: None,
        }
    }

    fn test_task(scan_id: &str) -> ScanTask {
        ScanTask {
            scan_id: scan_id.to_string(),
            scanner_name: "user_friction".to_string(),
            scanner_version: "1".to_string(),
            task_name: "friction".to_string(),
            status: TaskStatus::Pending,
            started: None,
            stopped: None,
            error: None,
        }
    }

    #[test]
    fn parse_metadata_variants() {
        let mut scan = test_scan();
        assert!(scan.parse_metadata().unwrap().is_none());

        scan.metadata = Some(
            serde_json::to_string(&ScanSummary {
                total: 3,
                completed: 2,
                failed: 1,
                skipped: 0,
                canceled: false,
                elapsed_ms: 1500,
            })
            .unwrap(),
        );
        match scan.parse_metadata().unwrap() {
            Some(ScanMetadata::Scan(s)) => assert_eq!(s.elapsed_ms, 1500),
            other => panic!("expected Scan variant, got {other:?}"),
        }

        scan.metadata = Some(
            serde_json::to_string(&AgentRunSummary {
                agent: "reconcile::reconcile".to_string(),
                elapsed_ms: 2500,
                is_error: false,
            })
            .unwrap(),
        );
        match scan.parse_metadata().unwrap() {
            Some(ScanMetadata::Agent(a)) => assert_eq!(a.elapsed_ms, 2500),
            other => panic!("expected Agent variant, got {other:?}"),
        }

        scan.metadata = Some(running_metadata(4242));
        match scan.parse_metadata().unwrap() {
            Some(ScanMetadata::Running(r)) => assert_eq!(r.pid, 4242),
            other => panic!("expected Running variant, got {other:?}"),
        }
    }

    #[test]
    fn insert_and_all() {
        let conn = open_db_in_memory().unwrap();
        let scan = test_scan();
        insert_scan(&conn, &scan).unwrap();

        let scans = all(&conn).unwrap();
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].id, "scan-001");
    }

    #[test]
    fn scan_attribution_follows_latest_writer() {
        let conn = open_db_in_memory().unwrap();
        for (id, created) in [("scan-a", 1000), ("scan-b", 2000), ("scan-c", 3000)] {
            let scan = Scan {
                id: id.to_string(),
                created,
                metadata: None,
            };
            insert_scan(&conn, &scan).unwrap();
        }
        let target = NoteTarget::Session(SessionTarget {
            session_id: "11111111-1111-1111-1111-111111111111".to_string(),
            line: None,
            line_end: None,
        });
        let note = Note::new(
            target,
            "finding",
            NoteValue(serde_json::Value::from("v")),
            "scanner:test",
        );
        note::insert(&conn, &note).unwrap();
        let scan_of = |note_id: &str| note::get(&conn, note_id).unwrap().scan;

        insert_scan_note(&conn, "scan-a", &note.id, ScanNoteRole::Wrote).unwrap();
        assert_eq!(scan_of(&note.id), Some("scan-a".to_string()));

        // A later scan replaces the value: attribution moves to it
        insert_scan_note(&conn, "scan-b", &note.id, ScanNoteRole::Wrote).unwrap();
        assert_eq!(scan_of(&note.id), Some("scan-b".to_string()));

        // A carried-forward link does not steal attribution
        insert_scan_note(&conn, "scan-c", &note.id, ScanNoteRole::Carried).unwrap();
        assert_eq!(scan_of(&note.id), Some("scan-b".to_string()));

        // A write in the same scan upgrades its carried link
        insert_scan_note(&conn, "scan-c", &note.id, ScanNoteRole::Wrote).unwrap();
        assert_eq!(scan_of(&note.id), Some("scan-c".to_string()));
    }

    #[test]
    fn task_lifecycle() {
        let conn = open_db_in_memory().unwrap();
        let scan = test_scan();
        insert_scan(&conn, &scan).unwrap();

        insert_task(&conn, &test_task(&scan.id)).unwrap();
        start_task(&conn, &scan.id, "user_friction", "friction", 1000).unwrap();
        finish_task(
            &conn,
            &scan.id,
            "user_friction",
            "friction",
            TaskStatus::Completed,
            Some(2500),
            None,
        )
        .unwrap();

        let tasks = tasks_for_scan(&conn, &scan.id).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].scanner_name, "user_friction");
        assert_eq!(tasks[0].scanner_version, "1");
        assert_eq!(tasks[0].status, TaskStatus::Completed);
        assert_eq!(tasks[0].started, Some(1000));
        assert_eq!(tasks[0].stopped, Some(2500));

        assert_eq!(
            scanner_names_for_scan(&conn, &scan.id).unwrap(),
            vec!["user_friction".to_string()]
        );
    }

    #[test]
    fn skipped_task_clears_timing() {
        let conn = open_db_in_memory().unwrap();
        let scan = test_scan();
        insert_scan(&conn, &scan).unwrap();

        insert_task(&conn, &test_task(&scan.id)).unwrap();
        start_task(&conn, &scan.id, "user_friction", "friction", 1000).unwrap();
        finish_task(
            &conn,
            &scan.id,
            "user_friction",
            "friction",
            TaskStatus::Skipped,
            None,
            None,
        )
        .unwrap();

        let tasks = tasks_for_scan(&conn, &scan.id).unwrap();
        assert_eq!(tasks[0].status, TaskStatus::Skipped);
        assert_eq!(tasks[0].started, None);
        assert_eq!(tasks[0].stopped, None);
    }

    #[test]
    fn task_agent_lifecycle() {
        let conn = open_db_in_memory().unwrap();
        let scan = test_scan();
        insert_scan(&conn, &scan).unwrap();
        insert_task(&conn, &test_task(&scan.id)).unwrap();

        insert_task_agent(
            &conn,
            &TaskAgent {
                session_id: "cccccccc-cccc-cccc-cccc-cccccccccccc".to_string(),
                scan_id: scan.id.clone(),
                scanner_name: "user_friction".to_string(),
                task_name: "friction".to_string(),
                exit_code: None,
                stderr: None,
                result: None,
            },
        )
        .unwrap();
        finish_task_agent(
            &conn,
            "cccccccc-cccc-cccc-cccc-cccccccccccc",
            0,
            "",
            Some(r#"{"type":"result","total_cost_usd":0.42}"#),
        )
        .unwrap();

        let (exit_code, result): (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT exit_code, result FROM task_agent WHERE session_id = ?1",
                params!["cccccccc-cccc-cccc-cccc-cccccccccccc"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(exit_code, Some(0));
        assert!(result.unwrap().contains("total_cost_usd"));

        let agents = agents_for_scan(&conn, &scan.id).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].session_id, "cccccccc-cccc-cccc-cccc-cccccccccccc");
        assert_eq!(agents[0].scanner_name, "user_friction");
        assert_eq!(agents[0].task_name, "friction");
        assert_eq!(agents[0].exit_code, Some(0));
    }

    #[test]
    fn cost_sums_agent_results_and_flags_missing_costs() {
        let conn = open_db_in_memory().unwrap();
        let scan = test_scan();
        insert_scan(&conn, &scan).unwrap();
        insert_task(&conn, &test_task(&scan.id)).unwrap();

        let cost = cost_for_scan(&conn, &scan.id).unwrap();
        assert_eq!(cost.total_usd, 0.0);
        assert!(!cost.incomplete);

        let agent = |session_id: &str| TaskAgent {
            session_id: session_id.to_string(),
            scan_id: scan.id.clone(),
            scanner_name: "user_friction".to_string(),
            task_name: "friction".to_string(),
            exit_code: None,
            stderr: None,
            result: None,
        };
        insert_task_agent(&conn, &agent("11111111-1111-1111-1111-111111111111")).unwrap();
        insert_task_agent(&conn, &agent("22222222-2222-2222-2222-222222222222")).unwrap();
        finish_task_agent(
            &conn,
            "11111111-1111-1111-1111-111111111111",
            0,
            "",
            Some(r#"{"type":"result","total_cost_usd":0.42}"#),
        )
        .unwrap();

        let cost = cost_for_scan(&conn, &scan.id).unwrap();
        assert_eq!(cost.total_usd, 0.42);
        assert!(cost.incomplete);

        finish_task_agent(
            &conn,
            "22222222-2222-2222-2222-222222222222",
            0,
            "",
            Some(r#"{"type":"result","total_cost_usd":0.08}"#),
        )
        .unwrap();

        let cost = cost_for_scan(&conn, &scan.id).unwrap();
        assert_eq!(cost.total_usd, 0.5);
        assert!(!cost.incomplete);
    }

    #[test]
    fn delete_scan_removes_run_metadata_but_keeps_notes() {
        let conn = open_db_in_memory().unwrap();
        let scan = test_scan();
        insert_scan(&conn, &scan).unwrap();
        insert_scan_session(
            &conn,
            &scan.id,
            "11111111-1111-1111-1111-111111111111",
            false,
        )
        .unwrap();

        insert_task(&conn, &test_task(&scan.id)).unwrap();

        let note = Note::new(
            NoteTarget::Session(
                SessionTarget::new("11111111-1111-1111-1111-111111111111").with_line(5),
            ),
            "scan.friction.score",
            NoteValue::from(1i64),
            "scanner:user_friction",
        );
        note::insert(&conn, &note).unwrap();

        delete_scan(&conn, &scan.id).unwrap();

        // Run metadata is gone
        assert_eq!(all(&conn).unwrap().len(), 0);
        assert_eq!(tasks_for_scan(&conn, &scan.id).unwrap().len(), 0);
        assert_eq!(session_ids_for_scan(&conn, &scan.id).unwrap().len(), 0);

        // Notes are not owned by a scan and survive
        let note_count: u32 = conn
            .query_row("SELECT COUNT(*) FROM note", [], |row| row.get(0))
            .unwrap();
        assert_eq!(note_count, 1);
    }

    #[test]
    fn delete_scan_not_found() {
        let conn = open_db_in_memory().unwrap();
        let result = delete_scan(&conn, "nonexistent");
        assert!(matches!(result, Err(ScanError::NotFound(_))));
    }

    #[test]
    fn scan_session_roundtrip() {
        let conn = open_db_in_memory().unwrap();
        let scan = test_scan();
        insert_scan(&conn, &scan).unwrap();
        insert_scan_session(
            &conn,
            &scan.id,
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            false,
        )
        .unwrap();
        insert_scan_session(
            &conn,
            &scan.id,
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            false,
        )
        .unwrap();
        let ids = session_ids_for_scan(&conn, &scan.id).unwrap();
        assert_eq!(
            ids,
            vec![
                "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string(),
                "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string()
            ]
        );
    }
}
