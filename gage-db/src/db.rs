use std::io;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use gage_core::config::gage_home;

pub const CURRENT_VERSION: u32 = 2;

#[derive(Debug)]
pub enum DbError {
    Io(io::Error),
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Io(e) => write!(f, "{e}"),
            DbError::Sqlite(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<io::Error> for DbError {
    fn from(e: io::Error) -> Self {
        DbError::Io(e)
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError::Sqlite(e)
    }
}

/// Path to the gage database. Honors `GAGE_DB`; otherwise the canonical
/// `<gage_home>/data/gage.db`. A single override covers every reader —
/// the datafusion tables and the MCP tools all resolve through here.
pub fn db_path() -> PathBuf {
    if let Some(p) = std::env::var_os("GAGE_DB") {
        return PathBuf::from(p);
    }
    gage_home().join("data").join("gage.db")
}

pub fn open_db() -> Result<Connection, DbError> {
    open_db_at(&db_path())
}

pub fn open_db_at(path: &Path) -> Result<Connection, DbError> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // synchronous=NORMAL is safe under WAL: durability of the most
    // recent commits depends on a checkpoint, but the database itself
    // can never be corrupted. Cuts fsync overhead by ~10x for
    // write-heavy workloads like scan runs.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    migrate(&conn)?;
    Ok(conn)
}

pub fn open_db_in_memory() -> Result<Connection, DbError> {
    let conn = Connection::open_in_memory()?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    let version = get_version(conn)?;
    if version >= CURRENT_VERSION {
        return Ok(());
    }
    if version < 1 {
        migration_1(conn)?;
    }
    if version < 2 {
        migration_2(conn)?;
    }
    set_version(conn, CURRENT_VERSION)
}

fn get_version(conn: &Connection) -> Result<u32, rusqlite::Error> {
    let has_table: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='schema_version'",
        [],
        |row| row.get(0),
    )?;
    if !has_table {
        return Ok(0);
    }
    conn.query_row("SELECT version FROM schema_version", [], |row| row.get(0))
}

fn set_version(conn: &Connection, version: u32) -> Result<(), rusqlite::Error> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")?;
    conn.execute("DELETE FROM schema_version", [])?;
    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        [version],
    )?;
    Ok(())
}

fn migration_1(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE note (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            target      TEXT NOT NULL,
            author      TEXT NOT NULL,
            value       TEXT NOT NULL,
            metadata    TEXT,
            explanation TEXT,
            created     INTEGER NOT NULL,
            modified    INTEGER
        );
        CREATE UNIQUE INDEX idx_note_duplicate_key ON note(name, target, author);
        CREATE INDEX idx_note_target ON note(target);
        CREATE INDEX idx_note_name   ON note(name);
        CREATE INDEX idx_note_author ON note(author);

        CREATE TABLE session_note (
            session_id TEXT NOT NULL,
            line       INTEGER,
            line_end   INTEGER,
            note_id    TEXT NOT NULL REFERENCES note(id),
            PRIMARY KEY (session_id, note_id)
        );

        CREATE TABLE project_note (
            project_path TEXT NOT NULL,
            note_id      TEXT NOT NULL REFERENCES note(id),
            PRIMARY KEY (project_path, note_id)
        );

        CREATE TABLE note_relation (
            note_id    TEXT NOT NULL REFERENCES note(id),
            related_to TEXT NOT NULL REFERENCES note(id),
            relation   TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (note_id, related_to, relation)
        );
        CREATE INDEX idx_note_relation_related_to ON note_relation (related_to);
        CREATE INDEX idx_note_relation_relation   ON note_relation (relation);

        CREATE TABLE issue (
            id            TEXT PRIMARY KEY,
            name          TEXT NOT NULL,
            target        TEXT NOT NULL,
            title         TEXT NOT NULL,
            description   TEXT,
            status        TEXT NOT NULL,
            closed_reason TEXT,
            created       INTEGER NOT NULL,
            modified      INTEGER,
            author        TEXT NOT NULL
        );
        CREATE UNIQUE INDEX idx_issue_duplicate_key ON issue(name, target);
        CREATE INDEX idx_issue_name   ON issue(name);
        CREATE INDEX idx_issue_status ON issue(status);

        CREATE TABLE issue_evidence (
            issue_id TEXT NOT NULL REFERENCES issue(id),
            note_id  TEXT NOT NULL REFERENCES note(id),
            name      TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            digest    TEXT,
            PRIMARY KEY (issue_id, note_id)
        );

        CREATE TABLE issue_event (
            issue_id  TEXT NOT NULL REFERENCES issue(id),
            type      TEXT NOT NULL,
            author    TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            value     TEXT
        );
        CREATE INDEX idx_issue_event_issue_id ON issue_event(issue_id);

        CREATE TABLE scan (
            id       TEXT PRIMARY KEY,
            created  INTEGER NOT NULL,
            metadata TEXT
        );

        CREATE TABLE scan_session (
            scan_id    TEXT NOT NULL REFERENCES scan(id),
            session_id TEXT NOT NULL,
            PRIMARY KEY (scan_id, session_id)
        );
        CREATE INDEX idx_scan_session_session_id ON scan_session(session_id);

        CREATE TABLE scan_scanner (
            id                   TEXT PRIMARY KEY,
            scan_id              TEXT NOT NULL REFERENCES scan(id),
            scanner_name         TEXT NOT NULL,
            scanner_version      TEXT NOT NULL,
            metadata             TEXT
        );

        CREATE TABLE scan_note (
            scan_id TEXT NOT NULL REFERENCES scan(id),
            note_id TEXT NOT NULL REFERENCES note(id),
            PRIMARY KEY (scan_id, note_id)
        );
        CREATE INDEX idx_scan_note_note_id ON scan_note(note_id);

        CREATE TABLE scan_issue (
            scan_id  TEXT NOT NULL REFERENCES scan(id),
            issue_id TEXT NOT NULL REFERENCES issue(id),
            PRIMARY KEY (scan_id, issue_id)
        );
        CREATE INDEX idx_scan_issue_issue_id ON scan_issue(issue_id);
",
    )?;
    Ok(())
}

fn migration_2(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE cache (
            key     TEXT PRIMARY KEY,
            value   TEXT NOT NULL,
            expires INTEGER
        );",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_creates_schema() {
        let conn = open_db_in_memory().unwrap();
        let version: u32 = conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_VERSION);

        // note has a single advisory target column
        let n: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('note') WHERE name = 'target'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "missing note column target");

        // the note dedup key is enforced by a unique index
        let n: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='index' AND name='idx_note_duplicate_key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "missing unique dedup index");

        // scan-related and note relation tables exist
        for tname in &[
            "scan_session",
            "scan",
            "session_note",
            "project_note",
            "scan_note",
            "scan_issue",
            "cache",
        ] {
            let n: u32 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [tname],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {tname}");
        }

        // issue and issue_evidence tables exist
        for tname in &["issue", "issue_evidence", "issue_event"] {
            let n: u32 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [tname],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {tname}");
        }
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = open_db_in_memory().unwrap();
        migrate(&conn).unwrap();
        let version: u32 = conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_VERSION);
    }
}
