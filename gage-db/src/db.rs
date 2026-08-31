use std::io;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use gage_core::config::gage_home;

pub const CURRENT_VERSION: u32 = 3;

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

/// Ensure the database exists and is migrated to the current schema.
///
/// For callers that read the database through something other than
/// [`open_db`] (e.g. the DataFusion sqlite connection pool), which
/// would otherwise fail on a fresh gage home.
pub fn ensure_db() -> Result<(), DbError> {
    open_db()?;
    Ok(())
}

pub fn open_db_at(path: &Path) -> Result<Connection, DbError> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    // Retry on SQLITE_BUSY: concurrent opens contend on the WAL
    // journal-mode switch, which does not invoke the busy handler
    // during the transition, so a timeout alone does not cover it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match try_open_db_at(path) {
            Err(DbError::Sqlite(e)) if is_busy(&e) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

fn try_open_db_at(path: &Path) -> Result<Connection, DbError> {
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // synchronous=NORMAL is safe under WAL: durability of the most
    // recent commits depends on a checkpoint, but the database itself
    // can never be corrupted. Cuts fsync overhead by ~10x for
    // write-heavy workloads like scan runs.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn is_busy(e: &rusqlite::Error) -> bool {
    e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
}

pub fn open_db_in_memory() -> Result<Connection, DbError> {
    let mut conn = Connection::open_in_memory()?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    // An immediate transaction serializes concurrent migrators (e.g.
    // parallel query-context creation on a fresh gage home): the
    // version is re-read under the write lock, so a second connection
    // sees the first one's completed migration and skips.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version = get_version(&tx)?;
    if version >= CURRENT_VERSION {
        return Ok(());
    }
    if version == 0 {
        // Fresh database: init_schema creates the current schema
        init_schema(&tx)?;
    } else {
        if version < 2 {
            tx.execute_batch("ALTER TABLE scan_task ADD COLUMN metadata TEXT")?;
        }
        if version < 3 {
            // Duplicate-key uniqueness moved from the schema to write
            // policies. The comment rename collapses the suffixes that
            // existed only to dodge the dropped constraint; it must
            // follow the index drops since it creates equal keys.
            tx.execute_batch(
                "DROP INDEX idx_note_duplicate_key;
                 DROP INDEX idx_issue_duplicate_key;
                 CREATE INDEX idx_note_key ON note(name, target, author);
                 CREATE INDEX idx_issue_key ON issue(name, author);
                 UPDATE note SET name = 'comment' WHERE name LIKE 'comment.%';",
            )?;
        }
    }
    set_version(&tx, CURRENT_VERSION)?;
    tx.commit()
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

// `ref` in task_validate is a prefixed entity reference
// (`session:<id>`, `note:<id>`); `value` is the optional compared
// validator (e.g. session size) and is NULL for membership schemes.
fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE note (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            target      TEXT NOT NULL,
            author      TEXT NOT NULL,
            value       TEXT NOT NULL,
            metadata    TEXT,
            created     INTEGER NOT NULL,
            modified    INTEGER
        );
        CREATE INDEX idx_note_key ON note(name, target, author);
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

        CREATE TABLE issue (
            id            TEXT PRIMARY KEY,
            name          TEXT NOT NULL,
            author        TEXT NOT NULL,
            title         TEXT NOT NULL,
            description   TEXT,
            target        TEXT,
            status        TEXT NOT NULL,
            status_reason TEXT,
            metadata      TEXT,
            created       INTEGER NOT NULL,
            modified      INTEGER
        );
        CREATE INDEX idx_issue_key ON issue(name, author);
        CREATE INDEX idx_issue_status ON issue(status);
        CREATE INDEX idx_issue_target ON issue(target);

        CREATE TABLE issue_evidence (
            issue_id TEXT NOT NULL REFERENCES issue(id),
            note_id  TEXT NOT NULL REFERENCES note(id),
            name      TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            digest    TEXT,
            PRIMARY KEY (issue_id, note_id)
        );

        CREATE TABLE session_issue (
            session_id TEXT NOT NULL,
            issue_id   TEXT NOT NULL REFERENCES issue(id),
            PRIMARY KEY (session_id, issue_id)
        );
        CREATE INDEX idx_session_issue_issue_id ON session_issue(issue_id);

        CREATE TABLE issue_event (
            issue_id  TEXT NOT NULL REFERENCES issue(id),
            type      TEXT NOT NULL,
            author    TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            metadata  TEXT
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
            metadata   TEXT,
            PRIMARY KEY (scan_id, session_id)
        );
        CREATE INDEX idx_scan_session_session_id ON scan_session(session_id);

        CREATE TABLE scan_task (
            scan_id         TEXT NOT NULL REFERENCES scan(id),
            scanner_name    TEXT NOT NULL,
            scanner_version TEXT NOT NULL,
            task_name       TEXT NOT NULL,
            status          TEXT NOT NULL,
            started         INTEGER,
            stopped         INTEGER,
            error           TEXT,
            metadata        TEXT,
            PRIMARY KEY (scan_id, scanner_name, task_name)
        );

        CREATE TABLE task_agent (
            session_id   TEXT PRIMARY KEY,
            scan_id      TEXT NOT NULL,
            scanner_name TEXT NOT NULL,
            task_name    TEXT NOT NULL,
            exit_code    INTEGER,
            stderr       TEXT,
            result       TEXT,
            FOREIGN KEY (scan_id, scanner_name, task_name)
                REFERENCES scan_task(scan_id, scanner_name, task_name)
        );
        CREATE INDEX idx_task_agent_task
            ON task_agent(scan_id, scanner_name, task_name);

        CREATE TABLE scan_note (
            scan_id TEXT NOT NULL REFERENCES scan(id),
            note_id TEXT NOT NULL REFERENCES note(id),
            role TEXT NOT NULL CHECK (role IN ('wrote', 'carried')),
            PRIMARY KEY (scan_id, note_id)
        );
        CREATE INDEX idx_scan_note_note_id ON scan_note(note_id);

        CREATE TABLE scan_issue (
            scan_id  TEXT NOT NULL REFERENCES scan(id),
            issue_id TEXT NOT NULL REFERENCES issue(id),
            PRIMARY KEY (scan_id, issue_id)
        );
        CREATE INDEX idx_scan_issue_issue_id ON scan_issue(issue_id);

        CREATE TABLE task_validate (
            key   TEXT NOT NULL,
            ref   TEXT NOT NULL,
            value TEXT,
            PRIMARY KEY (key, ref)
        );
",
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

        // no unique constraint on the note key; dedup is write policy
        let n: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index'
                 AND name='idx_note_key' AND sql NOT LIKE '%UNIQUE%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "missing non-unique note key index");

        // scan-related and note target tables exist
        for tname in &[
            "scan_session",
            "scan",
            "session_note",
            "project_note",
            "scan_note",
            "scan_issue",
            "task_validate",
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
        for tname in &["issue", "issue_evidence", "issue_event", "session_issue"] {
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
    fn migrate_v2_drops_unique_keys_and_renames_comments() {
        // A v2 database: current schema except for the unique duplicate
        // keys, plus a suffixed comment row predating the rename.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE note (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, target TEXT NOT NULL,
                author TEXT NOT NULL, value TEXT NOT NULL, metadata TEXT,
                created INTEGER NOT NULL, modified INTEGER
            );
            CREATE UNIQUE INDEX idx_note_duplicate_key ON note(name, target, author);
            CREATE TABLE issue (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, author TEXT NOT NULL,
                title TEXT NOT NULL, description TEXT, target TEXT,
                status TEXT NOT NULL, status_reason TEXT, metadata TEXT,
                created INTEGER NOT NULL, modified INTEGER
            );
            CREATE UNIQUE INDEX idx_issue_duplicate_key ON issue(name, author);
            INSERT INTO note VALUES
                ('n1', 'comment.abcd1234', 'session:s', 'user:g', '\"x\"', NULL, 1, NULL),
                ('n2', 'summary', 'session:s', 'scanner:s', '\"y\"', NULL, 2, NULL);",
        )
        .unwrap();
        set_version(&conn, 2).unwrap();

        migrate(&mut conn).unwrap();

        let name: String = conn
            .query_row("SELECT name FROM note WHERE id = 'n1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "comment");
        let unique_left: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index'
                 AND name IN ('idx_note_duplicate_key', 'idx_issue_duplicate_key')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unique_left, 0);

        // The dropped constraint no longer rejects an equal key
        conn.execute(
            "INSERT INTO note VALUES
                ('n3', 'summary', 'session:s', 'scanner:s', '\"z\"', NULL, 3, NULL)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn migrate_is_idempotent() {
        let mut conn = open_db_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let version: u32 = conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_VERSION);
    }
}
