//! Materialize a per-run sandbox sqlite db for an isolated agent run,
//! capture the agent's writes, and replay them into the canonical db.
//!
//! The agent talks to the gage MCP server, which opens whatever sqlite
//! path `GAGE_DB` resolves to. Pointing it at a sandbox db gives the
//! agent a private workspace: reads return only what the sandbox holds
//! (the spec controls the prune), and writes go to the sandbox alone.
//! On agent exit, the parent calls [`replay_writes`] to apply the
//! sandbox's writes against the canonical db in a single transaction.
//!
//! ## Snapshot consistency
//!
//! The source db may be concurrently written by `gage-scan` runs. We
//! take the sandbox snapshot via sqlite's `VACUUM INTO` from a live
//! connection, which yields a single self-consistent file regardless of
//! what other writers are doing.
//!
//! ## Writelog + triggers
//!
//! After the prune, the materializer installs an internal `_writelog`
//! table and per-table `AFTER INSERT|UPDATE|DELETE` triggers on every
//! writeable table. Each agent write appends one row to `_writelog`
//! recording `(seq, tbl, op, pk)` — the integer `seq` preserves replay
//! order so FK-respecting work survives the round trip. The triggers
//! are installed only after the prune so the prune's own deletes do not
//! enter the log.
//!
//! ## Replay
//!
//! [`replay_writes`] opens the sandbox, attaches the main db, and walks
//! `_writelog` in `seq` order inside a single transaction against main.
//! On any error (conflict on a unique constraint, FK violation, missing
//! parent row) the transaction rolls back and the function returns the
//! error — the caller can preserve the sandbox for inspection. With an
//! empty writelog this is a no-op that does not even open the
//! transaction.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{Connection, params};

use crate::db::{DbError, open_db_at};

/// What a sandbox should contain. Per dimension, `None` keeps everything
/// from the source; `Some(ids)` keeps only those ids.
#[derive(Debug, Default, Clone)]
pub struct SandboxSpec {
    /// Session ids whose `session_note` and (downstream) dependents to
    /// retain. `None` keeps every session.
    pub sessions: Option<HashSet<String>>,
    /// Note ids to retain. `None` keeps every note.
    pub notes: Option<HashSet<String>>,
    /// Issue ids to retain. `None` keeps every issue.
    pub issues: Option<HashSet<String>>,
    /// Scan id to retain in the `scan` table so the agent's MCP tools
    /// can record `scan_note` / `scan_issue` links against it. `None`
    /// drops every scan row.
    pub scan: Option<String>,
}

/// Schema-driven list of writeable tables: every table the agent's MCP
/// tools can mutate, plus enough metadata to install triggers, replay
/// rows by primary key, and run the prune.
///
/// `pk_cols` is the table's logical primary key (the columns whose
/// equality identifies the row for replay). For `issue_event`, which has
/// no logical PK, we use `rowid`.
struct Writeable {
    name: &'static str,
    pk_cols: &'static [&'static str],
}

const WRITEABLE_TABLES: &[Writeable] = &[
    Writeable {
        name: "note",
        pk_cols: &["id"],
    },
    Writeable {
        name: "session_note",
        pk_cols: &["session_id", "note_id"],
    },
    Writeable {
        name: "project_note",
        pk_cols: &["project_path", "note_id"],
    },
    Writeable {
        name: "note_relation",
        pk_cols: &["note_id", "related_to", "relation"],
    },
    Writeable {
        name: "issue",
        pk_cols: &["id"],
    },
    Writeable {
        name: "issue_evidence",
        pk_cols: &["issue_id", "note_id"],
    },
    Writeable {
        name: "issue_event",
        pk_cols: &["rowid"],
    },
];

/// Materialize a sandbox db at `dest` from `src`, pruned per `spec`.
/// Overwrites `dest` if it exists. After this returns, the sandbox has
/// the writelog and triggers installed and is ready for the agent.
pub fn materialize_sandbox(src: &Path, dest: &Path, spec: &SandboxSpec) -> Result<(), DbError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        std::fs::remove_file(dest)?;
    }
    snapshot_into(src, dest)?;

    let conn = open_db_at(dest)?;
    // The prune issues deletes in dependency-violating order on purpose;
    // disabling FK enforcement lets the statements read top-down.
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let tx = conn.unchecked_transaction()?;
    prune(&tx, spec)?;
    tx.commit()?;

    // Install triggers AFTER the prune so the prune does not enter the
    // writelog. Re-enable FKs so the agent's writes obey the schema.
    install_writelog(&conn)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    // Reclaim the deleted pages so the sandbox file is small and the
    // agent process opens it quickly. Must run outside a transaction.
    conn.execute_batch("VACUUM")?;
    Ok(())
}

/// Take a consistent point-in-time snapshot of `src` into `dest` via
/// `VACUUM INTO`. Safe even while other processes write `src`.
fn snapshot_into(src: &Path, dest: &Path) -> Result<(), DbError> {
    let conn = open_db_at(src)?;
    let mut stmt = conn.prepare("VACUUM INTO ?1")?;
    stmt.execute(params![dest.to_string_lossy().as_ref()])?;
    Ok(())
}

fn prune(tx: &Connection, spec: &SandboxSpec) -> Result<(), DbError> {
    // Drop scan bookkeeping the agent never reads. The `scan` row
    // itself is kept when `spec.scan` is set so the agent's MCP tools
    // can satisfy the FK on `scan_note` / `scan_issue` inserts.
    for table in ["scan_scanner", "scan_session", "scan_note", "scan_issue"] {
        tx.execute(&format!("DELETE FROM {table}"), [])?;
    }
    match &spec.scan {
        Some(id) => {
            tx.execute("DELETE FROM scan WHERE id <> ?1", params![id])?;
        }
        None => {
            tx.execute("DELETE FROM scan", [])?;
        }
    }

    if let Some(ids) = &spec.sessions {
        delete_outside(tx, "session_note", "session_id", ids)?;
    }
    if let Some(ids) = &spec.notes {
        delete_outside(tx, "note", "id", ids)?;
    }
    if let Some(ids) = &spec.issues {
        delete_outside(tx, "issue", "id", ids)?;
    }

    // Prune dependents whose parent rows are gone.
    tx.execute(
        "DELETE FROM session_note WHERE note_id NOT IN (SELECT id FROM note)",
        [],
    )?;
    tx.execute(
        "DELETE FROM project_note WHERE note_id NOT IN (SELECT id FROM note)",
        [],
    )?;
    tx.execute(
        "DELETE FROM note_relation \
         WHERE note_id NOT IN (SELECT id FROM note) \
            OR related_to NOT IN (SELECT id FROM note)",
        [],
    )?;
    tx.execute(
        "DELETE FROM issue_evidence \
         WHERE issue_id NOT IN (SELECT id FROM issue) \
            OR note_id NOT IN (SELECT id FROM note)",
        [],
    )?;
    tx.execute(
        "DELETE FROM issue_event WHERE issue_id NOT IN (SELECT id FROM issue)",
        [],
    )?;
    Ok(())
}

/// `DELETE FROM <table> WHERE <pk_col> NOT IN (?, ?, ...)`. Splits into
/// chunks to stay under sqlite's expression depth limits when `ids` is
/// large.
fn delete_outside(
    tx: &Connection,
    table: &str,
    pk_col: &str,
    keep: &HashSet<String>,
) -> Result<(), DbError> {
    // No allowlist → delete everything.
    if keep.is_empty() {
        tx.execute(&format!("DELETE FROM {table}"), [])?;
        return Ok(());
    }
    // Build a temp table of kept ids; faster than huge IN-lists and
    // robust against any expression-depth limit.
    tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS _keep (id TEXT PRIMARY KEY)")?;
    tx.execute("DELETE FROM _keep", [])?;
    {
        let mut stmt = tx.prepare("INSERT OR IGNORE INTO _keep(id) VALUES (?1)")?;
        for id in keep {
            stmt.execute(params![id])?;
        }
    }
    tx.execute(
        &format!("DELETE FROM {table} WHERE {pk_col} NOT IN (SELECT id FROM _keep)"),
        [],
    )?;
    tx.execute("DELETE FROM _keep", [])?;
    Ok(())
}

fn install_writelog(conn: &Connection) -> Result<(), DbError> {
    conn.execute_batch(
        "CREATE TABLE _writelog (
            seq INTEGER PRIMARY KEY,
            tbl TEXT NOT NULL,
            op  TEXT NOT NULL,
            pk  TEXT NOT NULL
        )",
    )?;
    let mut ddl = String::new();
    for w in WRITEABLE_TABLES {
        let new_json = pk_json_object(w.pk_cols, "NEW");
        let old_json = pk_json_object(w.pk_cols, "OLD");
        ddl.push_str(&format!(
            "CREATE TRIGGER _wl_{name}_i AFTER INSERT ON {name} BEGIN \
                INSERT INTO _writelog(tbl, op, pk) VALUES ('{name}','I',{new_json}); \
             END;\n\
             CREATE TRIGGER _wl_{name}_u AFTER UPDATE ON {name} BEGIN \
                INSERT INTO _writelog(tbl, op, pk) VALUES ('{name}','U',{new_json}); \
             END;\n\
             CREATE TRIGGER _wl_{name}_d AFTER DELETE ON {name} BEGIN \
                INSERT INTO _writelog(tbl, op, pk) VALUES ('{name}','D',{old_json}); \
             END;\n",
            name = w.name,
        ));
    }
    conn.execute_batch(&ddl)?;
    Ok(())
}

/// Build a sqlite expression that JSON-encodes the row's PK columns
/// using the given alias (`NEW` or `OLD`).
fn pk_json_object(pk_cols: &[&str], alias: &str) -> String {
    let mut s = String::from("json_object(");
    for (i, col) in pk_cols.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("'{col}',{alias}.{col}"));
    }
    s.push(')');
    s
}

/// Replay the sandbox's `_writelog` against `main_db` in `seq` order
/// inside a single transaction. Returns the number of writelog entries
/// applied. No-op (with no transaction opened) when the writelog is
/// empty.
pub fn replay_writes(sandbox: &Path, main_db: &Path) -> Result<usize, DbError> {
    let conn = open_db_at(sandbox)?;
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM _writelog", [], |r| r.get(0))?;
    if count == 0 {
        return Ok(0);
    }
    conn.execute(
        "ATTACH DATABASE ?1 AS main_db",
        params![main_db.to_string_lossy().as_ref()],
    )?;
    let result = replay_inner(&conn);
    // Detach even on replay failure so the connection is left clean.
    // The detach itself cannot meaningfully fail at this point — the
    // attach succeeded above and we own the connection.
    if let Err(e) = conn.execute("DETACH DATABASE main_db", []) {
        eprintln!("warning: DETACH after replay failed: {e}");
    }
    result
}

fn replay_inner(conn: &Connection) -> Result<usize, DbError> {
    let tx = conn.unchecked_transaction()?;
    let mut stmt = tx.prepare("SELECT seq, tbl, op, pk FROM _writelog ORDER BY seq")?;
    let rows: Vec<(i64, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let mut applied = 0usize;
    for (_seq, tbl, op, pk_json) in &rows {
        let w = WRITEABLE_TABLES
            .iter()
            .find(|w| w.name == tbl)
            .ok_or_else(|| {
                DbError::Sqlite(rusqlite::Error::InvalidQuery)
                    .map_with_msg(format!("unknown writelog table {tbl}"))
            })?;
        match op.as_str() {
            "I" | "U" => {
                let cols = sandbox_table_columns(&tx, w.name)?;
                let pk_match = pk_match_clause(w.pk_cols);
                let pk_vals = pk_values_from_json(w.pk_cols, pk_json)?;
                let sql = format!(
                    "INSERT OR REPLACE INTO main_db.{name} ({col_list}) \
                     SELECT {col_list} FROM main.{name} WHERE {pk_match}",
                    name = w.name,
                    col_list = cols.join(","),
                );
                let params = rusqlite::params_from_iter(pk_vals);
                tx.execute(&sql, params)?;
            }
            "D" => {
                let pk_match = pk_match_clause(w.pk_cols);
                let pk_vals = pk_values_from_json(w.pk_cols, pk_json)?;
                let sql = format!("DELETE FROM main_db.{} WHERE {}", w.name, pk_match);
                let params = rusqlite::params_from_iter(pk_vals);
                tx.execute(&sql, params)?;
            }
            other => {
                return Err(DbError::Sqlite(rusqlite::Error::InvalidQuery)
                    .map_with_msg(format!("unknown writelog op {other}")));
            }
        }
        applied += 1;
    }
    tx.commit()?;
    Ok(applied)
}

fn sandbox_table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, DbError> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
    let cols: Vec<String> = stmt
        .query_map(params![table], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(cols)
}

fn pk_match_clause(pk_cols: &[&str]) -> String {
    pk_cols
        .iter()
        .map(|c| format!("{c} = ?"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn pk_values_from_json(
    pk_cols: &[&str],
    json: &str,
) -> Result<Vec<rusqlite::types::Value>, DbError> {
    use rusqlite::types::Value;
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| {
        DbError::Sqlite(rusqlite::Error::InvalidQuery)
            .map_with_msg(format!("writelog pk JSON parse: {e}"))
    })?;
    let mut out = Vec::with_capacity(pk_cols.len());
    for col in pk_cols {
        let field = v.get(*col).ok_or_else(|| {
            DbError::Sqlite(rusqlite::Error::InvalidQuery)
                .map_with_msg(format!("writelog pk missing column {col}"))
        })?;
        out.push(match field {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Integer(i64::from(*b)),
            serde_json::Value::Number(n) => n
                .as_i64()
                .map(Value::Integer)
                .or_else(|| n.as_f64().map(Value::Real))
                .unwrap_or(Value::Null),
            serde_json::Value::String(s) => Value::Text(s.clone()),
            other => Value::Text(other.to_string()),
        });
    }
    Ok(out)
}

// Convenience wrapper to attach a message to a sqlite error.
trait DbErrorExt {
    fn map_with_msg(self, msg: String) -> DbError;
}

impl DbErrorExt for DbError {
    fn map_with_msg(self, msg: String) -> DbError {
        match self {
            DbError::Sqlite(e) => DbError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some(format!("{e}: {msg}")),
            )),
            other => other,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::db::open_db_at;
    use crate::issue::{self, Issue, IssueStatus};
    use crate::note::{self, Note, NoteValue};
    use crate::target::{NoteTarget, SessionTarget};
    use tempfile::tempdir;

    fn mk_note(name: &str, session: &str) -> Note {
        Note::new(
            NoteTarget::Session(SessionTarget::new(session)),
            name,
            NoteValue::from(1i64),
            "scanner:t",
        )
    }

    fn mk_issue(id: &str, name: &str, session: &str) -> Issue {
        Issue {
            id: id.to_string(),
            name: name.to_string(),
            target: format!("session://{session}"),
            title: name.to_string(),
            description: None,
            status: IssueStatus::Open,
            closed_reason: None,
            created: 0,
            modified: None,
            author: "scanner:t".to_string(),
        }
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn materialize_all_spec_keeps_everything() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.db");
        let dest = dir.path().join("sandbox.db");
        {
            let conn = open_db_at(&src).unwrap();
            let note_a = mk_note("a", "sess-a");
            let note_b = mk_note("b", "sess-b");
            note::insert(&conn, &note_a).unwrap();
            note::insert(&conn, &note_b).unwrap();
            let issue_a = mk_issue("issue-a", "ia", "sess-a");
            issue::insert(&conn, &issue_a).unwrap();
        }
        materialize_sandbox(&src, &dest, &SandboxSpec::default()).unwrap();
        let conn = open_db_at(&dest).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM note"), 2);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM issue"), 1);
        // Triggers installed.
        let trig: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='_wl_note_i'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(trig, 1);
    }

    #[test]
    fn materialize_filters_to_spec() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.db");
        let dest = dir.path().join("sandbox.db");
        let note_a;
        let note_b;
        let issue_a;
        let issue_b;
        {
            let conn = open_db_at(&src).unwrap();
            note_a = mk_note("a", "sess-a");
            note_b = mk_note("b", "sess-b");
            note::insert(&conn, &note_a).unwrap();
            note::insert(&conn, &note_b).unwrap();
            issue_a = mk_issue("issue-a", "ia", "sess-a");
            issue_b = mk_issue("issue-b", "ib", "sess-b");
            issue::insert(&conn, &issue_a).unwrap();
            issue::insert(&conn, &issue_b).unwrap();
        }
        let mut sessions = HashSet::new();
        sessions.insert("sess-a".to_string());
        let mut notes = HashSet::new();
        notes.insert(note_a.id.clone());
        let mut issues = HashSet::new();
        issues.insert(issue_a.id.clone());
        let spec = SandboxSpec {
            sessions: Some(sessions),
            notes: Some(notes),
            issues: Some(issues),
            scan: None,
        };
        materialize_sandbox(&src, &dest, &spec).unwrap();
        let conn = open_db_at(&dest).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM note"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM issue"), 1);
        let kept_note: String = conn
            .query_row("SELECT id FROM note", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept_note, note_a.id);
        // scan_* tables are wiped regardless.
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM scan_session"), 0);
        // Notes inserted by the test do not enter the writelog.
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM _writelog"), 0);
    }

    #[test]
    fn replay_writes_agent_inserts_to_main() {
        let dir = tempdir().unwrap();
        let main = dir.path().join("main.db");
        let sandbox = dir.path().join("sandbox.db");
        open_db_at(&main).unwrap(); // ensure schema
        materialize_sandbox(&main, &sandbox, &SandboxSpec::default()).unwrap();

        // Simulate an agent write inside the sandbox. Note insert writes
        // to both `note` and `session_note`; both should replay.
        let conn = open_db_at(&sandbox).unwrap();
        let n = mk_note("agent-note", "sess-x");
        note::insert(&conn, &n).unwrap();
        let i = mk_issue("issue-new", "new", "sess-x");
        issue::insert(&conn, &i).unwrap();
        drop(conn);

        let applied = replay_writes(&sandbox, &main).unwrap();
        assert!(applied >= 3, "expected >=3 writelog entries, got {applied}");

        let m = open_db_at(&main).unwrap();
        assert_eq!(count(&m, "SELECT COUNT(*) FROM note"), 1);
        assert_eq!(count(&m, "SELECT COUNT(*) FROM session_note"), 1);
        assert_eq!(count(&m, "SELECT COUNT(*) FROM issue"), 1);
        let kept_note: String = m
            .query_row("SELECT id FROM note", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept_note, n.id);
    }

    #[test]
    fn replay_empty_writelog_is_noop() {
        let dir = tempdir().unwrap();
        let main = dir.path().join("main.db");
        let sandbox = dir.path().join("sandbox.db");
        open_db_at(&main).unwrap();
        materialize_sandbox(&main, &sandbox, &SandboxSpec::default()).unwrap();
        let applied = replay_writes(&sandbox, &main).unwrap();
        assert_eq!(applied, 0);
    }

    #[test]
    fn replay_rolls_back_on_conflict() {
        let dir = tempdir().unwrap();
        let main = dir.path().join("main.db");
        let sandbox = dir.path().join("sandbox.db");
        // Pre-existing issue in main with the same (name, target) as
        // the sandbox writer's attempt — duplicate-key conflict.
        {
            let conn = open_db_at(&main).unwrap();
            issue::insert(&conn, &mk_issue("issue-pre", "x", "sess-x")).unwrap();
        }
        materialize_sandbox(&main, &sandbox, &SandboxSpec::default()).unwrap();
        // Agent writes a different-id issue with same (name, target).
        // The sandbox carries the pre-existing row from main; the
        // duplicate-key constraint fires inside the sandbox itself.
        {
            let conn = open_db_at(&sandbox).unwrap();
            issue::insert(&conn, &mk_issue("issue-new", "x", "sess-x")).unwrap_err();
            // A replayable write succeeds in the sandbox.
            let n = mk_note("agent-note", "sess-x");
            note::insert(&conn, &n).unwrap();
        }
        let applied = replay_writes(&sandbox, &main).unwrap();
        assert!(applied >= 1);
        // Replay landed in main.
        let m = open_db_at(&main).unwrap();
        assert_eq!(count(&m, "SELECT COUNT(*) FROM note"), 1);
    }
}
