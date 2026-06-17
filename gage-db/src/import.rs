//! Apply rows from another gage.db into this one.
//!
//! Policy: dest-wins. On a parent collision (matching `id` or matching
//! dedup key in `note`, `issue`, `scan`), the source row is rejected and
//! every source row that referenced it is also rejected. Child rows
//! whose own composite primary key already exists in dest are likewise
//! rejected. Nothing in dest is ever overwritten.
//!
//! Source schema is normalized before apply: equal version attaches
//! directly; older version is migrated against a copy under
//! `<gage_home>/tmp/`; newer version refuses. The source file itself is
//! treated as read-only.

use std::path::{Path, PathBuf};

use chrono::Local;
use rusqlite::{Connection, OpenFlags, Transaction, types::ValueRef};
use serde_json::{Map, Value};

use gage_core::config::gage_home;

use crate::db::{CURRENT_VERSION, DbError, db_path, open_db_at};

#[derive(Debug)]
pub enum ImportError {
    Db(DbError),
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    NewerSource { source: u32, dest: u32 },
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Db(e) => write!(f, "{e}"),
            ImportError::Sqlite(e) => write!(f, "{e}"),
            ImportError::Io(e) => write!(f, "{e}"),
            ImportError::Json(e) => write!(f, "{e}"),
            ImportError::NewerSource { source, dest } => {
                write!(
                    f,
                    "source schema version {source} is newer than dest {dest}"
                )
            }
        }
    }
}

impl std::error::Error for ImportError {}

impl From<DbError> for ImportError {
    fn from(e: DbError) -> Self {
        ImportError::Db(e)
    }
}
impl From<rusqlite::Error> for ImportError {
    fn from(e: rusqlite::Error) -> Self {
        ImportError::Sqlite(e)
    }
}
impl From<std::io::Error> for ImportError {
    fn from(e: std::io::Error) -> Self {
        ImportError::Io(e)
    }
}
impl From<serde_json::Error> for ImportError {
    fn from(e: serde_json::Error) -> Self {
        ImportError::Json(e)
    }
}

#[derive(Debug)]
pub struct TableReport {
    pub name: &'static str,
    pub accepted: u64,
    pub rejected: u64,
}

#[derive(Debug)]
pub struct ImportReport {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub preview: bool,
    pub tables: Vec<TableReport>,
    /// Path to the sidecar JSON file with rejected rows. None for preview
    /// runs and for real runs where nothing was rejected.
    pub rejected_path: Option<PathBuf>,
}

/// Apply `source` into the canonical gage database. When `preview` is
/// true the transaction is rolled back and no sidecar is written.
pub fn import(source: &Path, preview: bool) -> Result<ImportReport, ImportError> {
    import_at(source, &db_path(), preview)
}

/// Same as [`import`] but writes into an explicit dest path. Used by
/// tests and by callers that need to target a non-canonical database.
pub fn import_at(
    source: &Path,
    dest_path: &Path,
    preview: bool,
) -> Result<ImportReport, ImportError> {
    let (attach_path, tmp) = prepare_source(source)?;
    let result = run_with_attached(&attach_path, dest_path, preview);
    if let Some(p) = tmp {
        cleanup_tmp(&p)?;
    }
    let mut report = result?;
    report.source = source.to_path_buf();
    report.dest = dest_path.to_path_buf();
    report.preview = preview;
    Ok(report)
}

/// Returns (path to attach, tmp path to clean up after).
fn prepare_source(source: &Path) -> Result<(PathBuf, Option<PathBuf>), ImportError> {
    let version = read_source_version(source)?;
    if version > CURRENT_VERSION {
        return Err(ImportError::NewerSource {
            source: version,
            dest: CURRENT_VERSION,
        });
    }
    if version == CURRENT_VERSION {
        return Ok((source.to_path_buf(), None));
    }
    let tmp_dir = gage_home().join("tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp = tmp_dir.join(format!("import-{}.db", std::process::id()));
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }
    std::fs::copy(source, &tmp)?;
    // open_db_at migrates the copy to CURRENT_VERSION in place.
    drop(open_db_at(&tmp)?);
    Ok((tmp.clone(), Some(tmp)))
}

fn cleanup_tmp(tmp: &Path) -> Result<(), ImportError> {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = tmp.as_os_str().to_owned();
        p.push(suffix);
        let p = PathBuf::from(p);
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

fn read_source_version(source: &Path) -> Result<u32, ImportError> {
    let conn = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let has_table: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='schema_version'",
        [],
        |row| row.get(0),
    )?;
    if !has_table {
        return Ok(0);
    }
    let v: u32 = conn.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
    Ok(v)
}

fn run_with_attached(
    src_path: &Path,
    dest_path: &Path,
    preview: bool,
) -> Result<ImportReport, ImportError> {
    let mut dest = open_db_at(dest_path)?;
    dest.execute(
        "ATTACH DATABASE ?1 AS src",
        [src_path.to_string_lossy().as_ref()],
    )?;
    let result: Result<(Vec<TableReport>, Map<String, Value>), ImportError> = (|| {
        let tx = dest.transaction()?;
        build_accepted_parent_sets(&tx)?;
        let mut tables = Vec::new();
        let mut rejected = Map::new();
        for spec in TABLES {
            apply_table(&tx, spec, &mut tables, &mut rejected)?;
        }
        if preview {
            tx.rollback()?;
        } else {
            tx.commit()?;
        }
        Ok((tables, rejected))
    })();
    dest.execute("DETACH DATABASE src", [])?;
    let (tables, rejected) = result?;

    let mut rejected_path = None;
    let any_rejected = tables.iter().any(|t| t.rejected > 0);
    if !preview && any_rejected {
        let ts = Local::now().format("%Y%m%dT%H%M%S");
        let p = dest_path
            .parent()
            .unwrap()
            .join(format!("import-rejected-{ts}.json"));
        let doc = Value::Object({
            let mut m = Map::new();
            m.insert(
                "source".into(),
                Value::String(src_path.display().to_string()),
            );
            m.insert(
                "dest".into(),
                Value::String(dest_path.display().to_string()),
            );
            m.insert("timestamp".into(), Value::String(ts.to_string()));
            m.insert("tables".into(), Value::Object(rejected));
            m
        });
        std::fs::write(&p, serde_json::to_vec_pretty(&doc)?)?;
        rejected_path = Some(p);
    }

    Ok(ImportReport {
        source: src_path.to_path_buf(),
        dest: dest_path.to_path_buf(),
        preview,
        tables,
        rejected_path,
    })
}

fn build_accepted_parent_sets(tx: &Transaction) -> Result<(), ImportError> {
    tx.execute_batch(
        "CREATE TEMP TABLE accepted_note (id TEXT PRIMARY KEY);
         INSERT INTO accepted_note(id)
         SELECT s.id FROM src.note s
         WHERE s.id NOT IN (SELECT id FROM note)
           AND NOT EXISTS (
             SELECT 1 FROM note d
             WHERE d.name = s.name AND d.target = s.target AND d.author = s.author
           );

         CREATE TEMP TABLE accepted_issue (id TEXT PRIMARY KEY);
         INSERT INTO accepted_issue(id)
         SELECT s.id FROM src.issue s
         WHERE s.id NOT IN (SELECT id FROM issue)
           AND NOT EXISTS (
             SELECT 1 FROM issue d
             WHERE d.name = s.name AND d.target = s.target
           );

         CREATE TEMP TABLE accepted_scan (id TEXT PRIMARY KEY);
         INSERT INTO accepted_scan(id)
         SELECT s.id FROM src.scan s
         WHERE s.id NOT IN (SELECT id FROM scan);",
    )?;
    Ok(())
}

/// One table's apply spec. `accepted` is a SQL WHERE clause over an
/// alias `s` of `src.<name>` that selects the rows to be inserted into
/// dest; the rejection set is "every other row in src.<name>".
struct TableSpec {
    name: &'static str,
    accepted: &'static str,
}

const TABLES: &[TableSpec] = &[
    TableSpec {
        name: "note",
        accepted: "s.id IN (SELECT id FROM accepted_note)",
    },
    TableSpec {
        name: "issue",
        accepted: "s.id IN (SELECT id FROM accepted_issue)",
    },
    TableSpec {
        name: "scan",
        accepted: "s.id IN (SELECT id FROM accepted_scan)",
    },
    TableSpec {
        name: "session_note",
        accepted: "s.note_id IN (SELECT id FROM accepted_note) \
                   AND NOT EXISTS (SELECT 1 FROM session_note d \
                                   WHERE d.session_id = s.session_id AND d.note_id = s.note_id)",
    },
    TableSpec {
        name: "project_note",
        accepted: "s.note_id IN (SELECT id FROM accepted_note) \
                   AND NOT EXISTS (SELECT 1 FROM project_note d \
                                   WHERE d.project_path = s.project_path AND d.note_id = s.note_id)",
    },
    TableSpec {
        name: "note_relation",
        accepted: "s.note_id IN (SELECT id FROM accepted_note) \
                   AND s.related_to IN (SELECT id FROM accepted_note) \
                   AND NOT EXISTS (SELECT 1 FROM note_relation d \
                                   WHERE d.note_id = s.note_id \
                                     AND d.related_to = s.related_to \
                                     AND d.relation = s.relation)",
    },
    TableSpec {
        name: "issue_evidence",
        accepted: "s.issue_id IN (SELECT id FROM accepted_issue) \
                   AND s.note_id IN (SELECT id FROM accepted_note) \
                   AND NOT EXISTS (SELECT 1 FROM issue_evidence d \
                                   WHERE d.issue_id = s.issue_id AND d.note_id = s.note_id)",
    },
    TableSpec {
        name: "issue_event",
        // No primary key on issue_event; rule is parent-must-be-newly-
        // inserted so events on a pre-existing issue are dropped to
        // avoid duplication.
        accepted: "s.issue_id IN (SELECT id FROM accepted_issue)",
    },
    TableSpec {
        name: "scan_session",
        accepted: "s.scan_id IN (SELECT id FROM accepted_scan) \
                   AND NOT EXISTS (SELECT 1 FROM scan_session d \
                                   WHERE d.scan_id = s.scan_id AND d.session_id = s.session_id)",
    },
    TableSpec {
        name: "scan_scanner",
        accepted: "s.scan_id IN (SELECT id FROM accepted_scan) \
                   AND s.id NOT IN (SELECT id FROM scan_scanner)",
    },
];

fn apply_table(
    tx: &Transaction,
    spec: &TableSpec,
    tables: &mut Vec<TableReport>,
    rejected: &mut Map<String, Value>,
) -> Result<(), ImportError> {
    let rejected_rows = select_rows_as_json(
        tx,
        &format!(
            "SELECT s.* FROM src.{name} s WHERE NOT ({acc})",
            name = spec.name,
            acc = spec.accepted
        ),
    )?;
    let insert_sql = format!(
        "INSERT INTO {name} SELECT s.* FROM src.{name} s WHERE {acc}",
        name = spec.name,
        acc = spec.accepted
    );
    let accepted = tx.execute(&insert_sql, [])?;
    tables.push(TableReport {
        name: spec.name,
        accepted: accepted as u64,
        rejected: rejected_rows.len() as u64,
    });
    rejected.insert(spec.name.to_string(), Value::Array(rejected_rows));
    Ok(())
}

fn select_rows_as_json(conn: &Connection, sql: &str) -> Result<Vec<Value>, ImportError> {
    let mut stmt = conn.prepare(sql)?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut obj = Map::new();
        for (i, name) in cols.iter().enumerate() {
            let v = row.get_ref(i)?;
            let jv = match v {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(i) => Value::from(i),
                ValueRef::Real(f) => serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
                ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => Value::String(format!("<blob {} bytes>", b.len())),
            };
            obj.insert(name.clone(), jv);
        }
        out.push(Value::Object(obj));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn seed_source(path: &Path) {
        let conn = open_db_at(path).unwrap();
        conn.execute_batch(
            "INSERT INTO note (id, name, target, author, value, created) VALUES
                ('n1','quality','sess:a','scanner:x','1',100),
                ('n2','quality','sess:b','scanner:x','1',200),
                ('n3','quality','sess:c','scanner:y','1',300);
             INSERT INTO session_note (session_id, note_id) VALUES
                ('sess:a','n1'), ('sess:b','n2'), ('sess:c','n3');
             INSERT INTO issue (id, name, target, title, status, created, author) VALUES
                ('i1','dup','sess:a','t','open',100,'u');
             INSERT INTO issue_event (issue_id, type, author, timestamp) VALUES
                ('i1','open','u',100);",
        )
        .unwrap();
    }

    fn count(conn: &Connection, table: &str) -> u64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn empty_dest_takes_everything() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.db");
        let dest = dir.path().join("dest.db");
        seed_source(&src);

        // Point GAGE_DB at our dest, then call import.
        // SAFETY: tests run single-threaded for env mutation.
        let report = import_at(&src, &dest, false).unwrap();
        let dest_conn = open_db_at(&dest).unwrap();
        assert_eq!(count(&dest_conn, "note"), 3);
        assert_eq!(count(&dest_conn, "session_note"), 3);
        assert_eq!(count(&dest_conn, "issue"), 1);
        assert_eq!(count(&dest_conn, "issue_event"), 1);
        let note = report.tables.iter().find(|t| t.name == "note").unwrap();
        assert_eq!(note.accepted, 3);
        assert_eq!(note.rejected, 0);
        assert!(report.rejected_path.is_none());
    }

    #[test]
    fn dedup_key_collision_rejects_row_and_children() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.db");
        let dest = dir.path().join("dest.db");
        seed_source(&src);

        unsafe { std::env::set_var("GAGE_DB", &dest) };
        {
            let dest_conn = open_db_at(&dest).unwrap();
            // Pre-populate dest with a colliding note: same dedup key
            // (quality, sess:a, scanner:x) but different id.
            dest_conn
                .execute(
                    "INSERT INTO note (id, name, target, author, value, created)
                     VALUES ('local-1','quality','sess:a','scanner:x','9',50)",
                    [],
                )
                .unwrap();
        }
        let report = import(&src, false).unwrap();
        let dest_conn = open_db_at(&dest).unwrap();
        // n1 should be rejected (collision); n2, n3 accepted.
        assert_eq!(count(&dest_conn, "note"), 3);
        // session_note for n1 is dropped (parent rejected).
        assert_eq!(count(&dest_conn, "session_note"), 2);
        let note = report.tables.iter().find(|t| t.name == "note").unwrap();
        assert_eq!(note.accepted, 2);
        assert_eq!(note.rejected, 1);
        let sn = report
            .tables
            .iter()
            .find(|t| t.name == "session_note")
            .unwrap();
        assert_eq!(sn.accepted, 2);
        assert_eq!(sn.rejected, 1);
        assert!(report.rejected_path.is_some());
        let rp = report.rejected_path.unwrap();
        let blob: Value = serde_json::from_slice(&std::fs::read(&rp).unwrap()).unwrap();
        let n = blob
            .get("tables")
            .and_then(|v| v.get("note"))
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(n.len(), 1);
        assert_eq!(n.first().and_then(|r| r.get("id")).unwrap(), "n1");
    }

    #[test]
    fn preview_does_not_write() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.db");
        let dest = dir.path().join("dest.db");
        seed_source(&src);

        let report = import_at(&src, &dest, true).unwrap();
        assert!(report.rejected_path.is_none());
        let dest_conn = open_db_at(&dest).unwrap();
        assert_eq!(count(&dest_conn, "note"), 0);
        let note = report.tables.iter().find(|t| t.name == "note").unwrap();
        assert_eq!(note.accepted, 3);
    }

    #[test]
    fn newer_source_refuses() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.db");
        let dest = dir.path().join("dest.db");
        seed_source(&src);
        // Bump source past CURRENT_VERSION.
        let c = Connection::open(&src).unwrap();
        c.execute(
            "UPDATE schema_version SET version = ?1",
            [CURRENT_VERSION + 1],
        )
        .unwrap();
        drop(c);

        let err = import_at(&src, &dest, true).unwrap_err();
        assert!(matches!(err, ImportError::NewerSource { .. }));
    }
}
