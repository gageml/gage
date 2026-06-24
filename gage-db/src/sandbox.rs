//! Materialize a per-scan sqlite db for an isolated agent run.
//!
//! The judge agent talks to the gage MCP server, which opens whatever
//! sqlite path `GAGE_DB` resolves to. To restrict what the judge can see
//! to the rows associated with one scan, we copy the source db to a
//! throwaway file and delete everything outside that scan's
//! `scan_session` / `scan_note` / `scan_issue` sets — plus their
//! dependents. The MCP server then physically cannot return out-of-scope
//! rows, so the runtime engine needs no scope wiring of its own.
//!
//! The file-copy + delete approach (rather than schema-aware
//! `INSERT … SELECT`) keeps this code agnostic to columns added to
//! existing tables; only new tables need to be considered here.

use std::path::Path;

use rusqlite::params;

use crate::db::{DbError, open_db_at};

/// Build a sandbox db at `dest` containing only the rows associated with
/// `scan_id` in `src`. Overwrites `dest` if it exists.
pub fn materialize_scan_sandbox(src: &Path, dest: &Path, scan_id: &str) -> Result<(), DbError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        std::fs::remove_file(dest)?;
    }
    std::fs::copy(src, dest)?;

    let conn = open_db_at(dest)?;
    // Deletes here would otherwise need a topological ordering to avoid
    // transient FK violations within the transaction. The end state is
    // FK-consistent regardless; turning the check off for the duration
    // of the prune lets the statements run in a readable order.
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let tx = conn.unchecked_transaction()?;

    // Scan-keyed tables: keep only the requested scan.
    for table in [
        "scan",
        "scan_scanner",
        "scan_session",
        "scan_note",
        "scan_issue",
    ] {
        let sql = if table == "scan" {
            "DELETE FROM scan WHERE id != ?1".to_string()
        } else {
            format!("DELETE FROM {table} WHERE scan_id != ?1")
        };
        tx.execute(&sql, params![scan_id])?;
    }

    // Notes and issues: keep only what the scan recorded via scan_note /
    // scan_issue. Then prune dependents whose parent rows are gone.
    tx.execute(
        "DELETE FROM note WHERE id NOT IN (SELECT note_id FROM scan_note)",
        [],
    )?;
    tx.execute(
        "DELETE FROM issue WHERE id NOT IN (SELECT issue_id FROM scan_issue)",
        [],
    )?;
    tx.execute(
        "DELETE FROM session_note \
         WHERE note_id NOT IN (SELECT id FROM note) \
            OR session_id NOT IN (SELECT session_id FROM scan_session)",
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

    tx.commit()?;

    // Reclaim the deleted pages so the sandbox file is small and the
    // judge process opens it quickly. Must run outside a transaction.
    conn.execute_batch("VACUUM")?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::db::open_db_at;
    use crate::issue::{self, Issue, IssueStatus};
    use crate::note::{self, Note, NoteValue};
    use crate::scan::{
        Scan, insert_scan, insert_scan_issue, insert_scan_note, insert_scan_session,
    };
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

    fn mk_issue(name: &str, session: &str) -> Issue {
        Issue {
            id: format!("issue-{name}-{session}"),
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

    #[test]
    fn keeps_only_in_scope_rows() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.db");
        let dest = dir.path().join("sandbox.db");

        {
            let conn = open_db_at(&src).unwrap();
            for id in ["scan-a", "scan-b"] {
                insert_scan(
                    &conn,
                    &Scan {
                        id: id.to_string(),
                        created: 0,
                        metadata: None,
                    },
                )
                .unwrap();
            }
            insert_scan_session(&conn, "scan-a", "sess-a").unwrap();
            insert_scan_session(&conn, "scan-b", "sess-b").unwrap();

            let note_a = mk_note("a-note", "sess-a");
            let note_b = mk_note("b-note", "sess-b");
            note::insert(&conn, &note_a).unwrap();
            note::insert(&conn, &note_b).unwrap();
            insert_scan_note(&conn, "scan-a", &note_a.id).unwrap();
            insert_scan_note(&conn, "scan-b", &note_b.id).unwrap();

            let issue_a = mk_issue("a-iss", "sess-a");
            let issue_b = mk_issue("b-iss", "sess-b");
            issue::insert(&conn, &issue_a).unwrap();
            issue::insert(&conn, &issue_b).unwrap();
            insert_scan_issue(&conn, "scan-a", &issue_a.id).unwrap();
            insert_scan_issue(&conn, "scan-b", &issue_b.id).unwrap();
        }

        materialize_scan_sandbox(&src, &dest, "scan-a").unwrap();

        let conn = open_db_at(&dest).unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT COUNT(*) FROM scan"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM scan_session"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM scan_note"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM scan_issue"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM note"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM issue"), 1);
        let kept_scan: String = conn
            .query_row("SELECT id FROM scan", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept_scan, "scan-a");
    }

    #[test]
    fn overwrites_existing_dest() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.db");
        let dest = dir.path().join("sandbox.db");

        {
            let conn = open_db_at(&src).unwrap();
            insert_scan(
                &conn,
                &Scan {
                    id: "scan-a".to_string(),
                    created: 0,
                    metadata: None,
                },
            )
            .unwrap();
        }
        std::fs::write(&dest, b"garbage").unwrap();
        materialize_scan_sandbox(&src, &dest, "scan-a").unwrap();
        let conn = open_db_at(&dest).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM scan", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
