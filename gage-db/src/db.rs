use std::path::PathBuf;

use rusqlite::Connection;

use gage_core::config::gage_home;

const CURRENT_VERSION: u32 = 1;

pub fn db_path() -> PathBuf {
    gage_home().join("data").join("gage.db")
}

pub fn open_db() -> Connection {
    open_db_at(&db_path())
}

pub fn open_db_at(path: &std::path::Path) -> Connection {
    std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create data directory");
    let conn = Connection::open(path).expect("failed to open gage.db");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("failed to enable WAL mode");
    // synchronous=NORMAL is safe under WAL: durability of the most
    // recent commits depends on a checkpoint, but the database itself
    // can never be corrupted. Cuts fsync overhead by ~10x for
    // write-heavy workloads like scan runs.
    conn.pragma_update(None, "synchronous", "NORMAL")
        .expect("failed to set synchronous=NORMAL");
    migrate(&conn);
    conn
}

pub fn open_db_in_memory() -> Connection {
    let conn = Connection::open_in_memory().expect("failed to open in-memory database");
    migrate(&conn);
    conn
}

fn migrate(conn: &Connection) {
    let version = get_version(conn);
    if version >= CURRENT_VERSION {
        return;
    }
    if version < 1 {
        migration_1(conn);
    }
    set_version(conn, CURRENT_VERSION);
}

fn get_version(conn: &Connection) -> u32 {
    let has_table: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("failed to check for schema_version table");
    if !has_table {
        return 0;
    }
    conn.query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .expect("failed to read schema version")
}

fn set_version(conn: &Connection, version: u32) {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")
        .expect("failed to create schema_version table");
    conn.execute("DELETE FROM schema_version", [])
        .expect("failed to clear schema_version");
    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        [version],
    )
    .expect("failed to set schema version");
}

fn migration_1(conn: &Connection) {
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
",
    )
    .expect("migration 1 failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_creates_schema() {
        let conn = open_db_in_memory();
        let version: u32 = conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);

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
        for tname in &["scan_session", "scan", "session_note", "project_note"] {
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
        let conn = open_db_in_memory();
        migrate(&conn);
        let version: u32 = conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }
}
