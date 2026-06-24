use rusqlite::{Connection, params};

#[derive(Debug, Clone)]
pub struct Scan {
    pub id: String,
    /// Epoch milliseconds.
    pub created: i64,
    pub metadata: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ScanScanner {
    pub id: String,
    pub scan_id: String,
    pub scanner_name: String,
    pub scanner_version: String,
    pub metadata: Option<String>,
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

pub fn insert_scanner(conn: &Connection, scanner: &ScanScanner) -> Result<(), ScanError> {
    conn.execute(
        "INSERT INTO scan_scanner (id, scan_id, scanner_name, scanner_version, metadata) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![scanner.id, scanner.scan_id, scanner.scanner_name, scanner.scanner_version, scanner.metadata],
    )?;
    Ok(())
}

/// Record that `session_id` was selected for `scan_id`.
pub fn insert_scan_session(
    conn: &Connection,
    scan_id: &str,
    session_id: &str,
) -> Result<(), ScanError> {
    conn.execute(
        "INSERT INTO scan_session (scan_id, session_id) VALUES (?1, ?2)",
        params![scan_id, session_id],
    )?;
    Ok(())
}

/// Record that `note_id` was written during `scan_id`. A repeat write of
/// the same note in the same scan is a no-op.
pub fn insert_scan_note(conn: &Connection, scan_id: &str, note_id: &str) -> Result<(), ScanError> {
    conn.execute(
        "INSERT OR IGNORE INTO scan_note (scan_id, note_id) VALUES (?1, ?2)",
        params![scan_id, note_id],
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

pub fn get_scanners_for_scan(
    conn: &Connection,
    scan_id: &str,
) -> Result<Vec<ScanScanner>, ScanError> {
    let mut stmt = conn.prepare(
        "SELECT id, scan_id, scanner_name, scanner_version, metadata FROM scan_scanner WHERE scan_id = ?1",
    )?;
    let scanners = stmt
        .query_map(params![scan_id], |row| {
            Ok(ScanScanner {
                id: row.get(0)?,
                scan_id: row.get(1)?,
                scanner_name: row.get(2)?,
                scanner_version: row.get(3)?,
                metadata: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(scanners)
}

/// Delete a scan's run metadata: the `scan` row plus its `scan_scanner`
/// and `scan_session` rows. Notes and issues are refreshed across scans
/// and are not owned by any one scan, so they are never deleted here.
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
        "DELETE FROM scan_scanner WHERE scan_id = ?1",
        params![scan_id],
    )?;
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

    fn test_scanner(scan_id: &str) -> ScanScanner {
        ScanScanner {
            id: "scanner-001".to_string(),
            scan_id: scan_id.to_string(),
            scanner_name: "user_friction".to_string(),
            scanner_version: "1".to_string(),
            metadata: None,
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
    fn insert_and_get_scanners() {
        let conn = open_db_in_memory().unwrap();
        let scan = test_scan();
        insert_scan(&conn, &scan).unwrap();

        let scanner = test_scanner(&scan.id);
        insert_scanner(&conn, &scanner).unwrap();

        let scanners = get_scanners_for_scan(&conn, &scan.id).unwrap();
        assert_eq!(scanners.len(), 1);
        assert_eq!(scanners[0].scanner_name, "user_friction");
        assert_eq!(scanners[0].scanner_version, "1");
    }

    #[test]
    fn delete_scan_removes_run_metadata_but_keeps_notes() {
        let conn = open_db_in_memory().unwrap();
        let scan = test_scan();
        insert_scan(&conn, &scan).unwrap();
        insert_scan_session(&conn, &scan.id, "sess-1").unwrap();

        let scanner = test_scanner(&scan.id);
        insert_scanner(&conn, &scanner).unwrap();

        let note = Note::new(
            NoteTarget::Session(SessionTarget::new("sess-1").with_line(5)),
            "scan.friction.score",
            NoteValue::from(1i64),
            "scanner:user_friction",
        );
        note::insert(&conn, &note).unwrap();

        delete_scan(&conn, &scan.id).unwrap();

        // Run metadata is gone
        assert_eq!(all(&conn).unwrap().len(), 0);
        assert_eq!(get_scanners_for_scan(&conn, &scan.id).unwrap().len(), 0);
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
        insert_scan_session(&conn, &scan.id, "sess-a").unwrap();
        insert_scan_session(&conn, &scan.id, "sess-b").unwrap();
        let ids = session_ids_for_scan(&conn, &scan.id).unwrap();
        assert_eq!(ids, vec!["sess-a".to_string(), "sess-b".to_string()]);
    }
}
