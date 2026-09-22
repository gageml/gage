//! [`SqliteIndex`]: the [`ObjectIndex`] implementation, a SQLite file
//! under Gage's cache directory.
//!
//! Schema:
//!
//! ```text
//! meta   (schema_version)
//! ref    (id PRIMARY KEY, tip_sha)
//! object (sha PRIMARY KEY, id, type, version, created, modified, deleted, parent_sha)
//! link   (commit_sha, link_file, ord, target_sha)
//! attr   (commit_sha, key, value)
//! ```
//!
//! `ref` is what the reconcile diff runs against. `object` holds every
//! commit the index has seen, so a linked SHA resolves after its ref
//! has moved on; a commit that a delete made unreachable stays until
//! `gc` prunes it, as in git. `link` serves reverse lookups. `attr`
//! holds the values each type opted in through `INDEXED_ATTRS`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::StoreError;
use crate::index::{IdMatch, ObjectIndex, ObjectQuery, Order, SelectedTip};
use crate::object::{LinkFile, Object};

/// Bumped when the schema changes. A mismatch discards the file and
/// rebuilds from an empty ref table.
pub const INDEX_SCHEMA_VERSION: u32 = 2;

const SCHEMA: &str = "
CREATE TABLE meta (schema_version INTEGER NOT NULL);
CREATE TABLE ref (id TEXT PRIMARY KEY, tip_sha TEXT NOT NULL);
CREATE TABLE object (
    sha TEXT PRIMARY KEY,
    id TEXT NOT NULL,
    type TEXT NOT NULL,
    version TEXT NOT NULL,
    created INTEGER,
    modified INTEGER,
    deleted INTEGER,
    parent_sha TEXT
);
CREATE INDEX object_type_created ON object (type, created);
CREATE INDEX object_type_modified ON object (type, modified);
CREATE INDEX object_modified ON object (modified);
CREATE TABLE link (
    commit_sha TEXT NOT NULL,
    link_file TEXT NOT NULL,
    ord INTEGER NOT NULL,
    target_sha TEXT NOT NULL,
    PRIMARY KEY (commit_sha, link_file, ord)
);
CREATE INDEX link_target ON link (target_sha);
CREATE TABLE attr (
    commit_sha TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (commit_sha, key)
);
CREATE INDEX attr_key_value ON attr (key, value);
";

pub struct SqliteIndex {
    conn: Connection,
}

impl SqliteIndex {
    /// Open the index at `path`, creating it when absent and rebuilding
    /// it when its schema version differs from
    /// [`INDEX_SCHEMA_VERSION`]. A rebuilt index starts with an empty
    /// ref table, which makes the next reconcile a full walk.
    pub fn open(path: &Path) -> Result<SqliteIndex, StoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| index_err(format!("create {}: {e}", parent.display())))?;
        }
        if path.exists() && !schema_matches(path)? {
            fs::remove_file(path)
                .map_err(|e| index_err(format!("remove {}: {e}", path.display())))?;
        }
        let fresh = !path.exists();
        let conn = Connection::open(path).map_err(sql_err)?;
        // Readers do not block the writer under WAL, so a TUI can read
        // while a CLI writes.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_err)?;
        // The index is rebuilt from the refs whenever it is missing or
        // damaged, so a commit does not need to reach disk before the
        // call returns; NORMAL under WAL skips the per-commit sync.
        // Together with one transaction per reconcile this is the
        // change measured in footnote 2 of
        // gage-bench/results/store/README.md.
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_err)?;
        // A concurrent writer holds the reserved lock for the duration
        // of its transaction. Wait rather than fail so `Store::open`
        // and `index_written` do not surface a spurious "database is
        // locked" when a TUI, CLI, or scan runs alongside. Matches
        // rusqlite's current default, but set it here so the store does
        // not depend on that default.
        conn.busy_timeout(Duration::from_millis(5000))
            .map_err(sql_err)?;
        if fresh {
            conn.execute_batch(SCHEMA).map_err(sql_err)?;
            conn.execute(
                "INSERT INTO meta (schema_version) VALUES (?1)",
                params![INDEX_SCHEMA_VERSION],
            )
            .map_err(sql_err)?;
        }
        Ok(SqliteIndex { conn })
    }
}

/// True when the file at `path` carries the current schema version.
/// Any failure to read it counts as a mismatch, so a corrupt file is
/// rebuilt rather than reported.
fn schema_matches(path: &Path) -> Result<bool, StoreError> {
    let Ok(conn) = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return Ok(false);
    };
    let version: Result<u32, _> =
        conn.query_row("SELECT schema_version FROM meta", [], |row| row.get(0));
    Ok(version.map(|v| v == INDEX_SCHEMA_VERSION).unwrap_or(false))
}

impl ObjectIndex for SqliteIndex {
    fn tips(&self) -> Result<BTreeMap<String, String>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, tip_sha FROM ref")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sql_err)?;
        let mut tips = BTreeMap::new();
        for row in rows {
            let (id, sha) = row.map_err(sql_err)?;
            tips.insert(id, sha);
        }
        Ok(tips)
    }

    fn has_commit(&self, sha: &str) -> Result<bool, StoreError> {
        let found: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM object WHERE sha = ?1", params![sha], |row| {
                row.get(0)
            })
            .optional()
            .map_err(sql_err)?;
        Ok(found.is_some())
    }

    fn put(
        &self,
        object: &Object,
        links: &[LinkFile],
        attrs: &[(&'static str, String)],
    ) -> Result<(), StoreError> {
        let h = &object.header;
        let sha = &object.commit_sha;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO object
                 (sha, id, type, version, created, modified, deleted, parent_sha)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    sha,
                    h.id,
                    h.object_type,
                    h.version,
                    h.created_ms,
                    h.modified_ms,
                    h.deleted_ms,
                    h.parent
                ],
            )
            .map_err(sql_err)?;
        self.conn
            .execute("DELETE FROM link WHERE commit_sha = ?1", params![sha])
            .map_err(sql_err)?;
        for file in links {
            for (ord, target) in file.shas.iter().enumerate() {
                self.conn
                    .execute(
                        "INSERT INTO link (commit_sha, link_file, ord, target_sha)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![sha, file.path, ord as i64, target],
                    )
                    .map_err(sql_err)?;
            }
        }
        self.conn
            .execute("DELETE FROM attr WHERE commit_sha = ?1", params![sha])
            .map_err(sql_err)?;
        for (key, value) in attrs {
            self.conn
                .execute(
                    "INSERT INTO attr (commit_sha, key, value) VALUES (?1, ?2, ?3)",
                    params![sha, key, value],
                )
                .map_err(sql_err)?;
        }
        Ok(())
    }

    /// Every index transaction writes, so the reserved lock is taken
    /// at BEGIN. A deferred transaction would read under a snapshot
    /// and fail with `SQLITE_BUSY_SNAPSHOT` on the first write if
    /// another connection committed in between; that failure is not
    /// resolved by `busy_timeout`.
    fn begin(&self) -> Result<(), StoreError> {
        self.conn.execute_batch("BEGIN IMMEDIATE").map_err(sql_err)
    }

    fn commit(&self) -> Result<(), StoreError> {
        self.conn.execute_batch("COMMIT").map_err(sql_err)
    }

    fn rollback(&self) -> Result<(), StoreError> {
        self.conn.execute_batch("ROLLBACK").map_err(sql_err)
    }

    /// Reachability is computed inside SQLite: from every tip, through
    /// `parent_sha` and every link target, the same edges a rebuild
    /// walks. Rows for anything else are removed.
    fn prune(&self) -> Result<usize, StoreError> {
        let removed = self
            .conn
            .execute(
                "WITH RECURSIVE reachable(sha) AS (
                     SELECT tip_sha FROM ref
                     UNION
                     SELECT o.parent_sha FROM object o
                         JOIN reachable r ON o.sha = r.sha
                         WHERE o.parent_sha IS NOT NULL
                     UNION
                     SELECT l.target_sha FROM link l
                         JOIN reachable r ON l.commit_sha = r.sha
                 )
                 DELETE FROM object WHERE sha NOT IN (SELECT sha FROM reachable)",
                [],
            )
            .map_err(sql_err)?;
        self.conn
            .execute(
                "DELETE FROM link WHERE commit_sha NOT IN (SELECT sha FROM object)",
                [],
            )
            .map_err(sql_err)?;
        self.conn
            .execute(
                "DELETE FROM attr WHERE commit_sha NOT IN (SELECT sha FROM object)",
                [],
            )
            .map_err(sql_err)?;
        Ok(removed)
    }

    fn set_tip(&self, id: &str, tip: Option<&str>) -> Result<(), StoreError> {
        match tip {
            Some(sha) => self
                .conn
                .execute(
                    "INSERT OR REPLACE INTO ref (id, tip_sha) VALUES (?1, ?2)",
                    params![id, sha],
                )
                .map_err(sql_err)?,
            None => self
                .conn
                .execute("DELETE FROM ref WHERE id = ?1", params![id])
                .map_err(sql_err)?,
        };
        Ok(())
    }

    fn select(&self, query: &ObjectQuery) -> Result<Vec<SelectedTip>, StoreError> {
        let mut sql = String::from(
            "SELECT o.id, o.sha, o.created, o.modified FROM ref r JOIN object o ON o.sha = r.tip_sha",
        );
        let mut args: Vec<String> = Vec::new();
        for (i, (key, value)) in query.attrs.iter().enumerate() {
            let k = args.len() + 1;
            sql.push_str(&format!(
                " JOIN attr a{i} ON a{i}.commit_sha = o.sha AND a{i}.key = ?{k} AND a{i}.value = ?{}",
                k + 1
            ));
            args.push((*key).to_string());
            args.push(value.clone());
        }
        sql.push_str(&format!(
            " WHERE o.type = ?{} AND o.deleted IS NULL",
            args.len() + 1
        ));
        args.push(query.object_type.to_string());
        sql.push_str(match query.order {
            Order::CreatedAsc => " ORDER BY o.created ASC, o.sha ASC",
            Order::CreatedDesc => " ORDER BY o.created DESC, o.sha ASC",
            Order::ModifiedAsc => " ORDER BY o.modified ASC, o.sha ASC",
            Order::ModifiedDesc => " ORDER BY o.modified DESC, o.sha ASC",
        });
        if let Some(limit) = query.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        let mut stmt = self.conn.prepare(&sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |row| {
                Ok(SelectedTip {
                    id: row.get(0)?,
                    sha: row.get(1)?,
                    created_ms: row.get(2)?,
                    modified_ms: row.get(3)?,
                })
            })
            .map_err(sql_err)?;
        let mut tips = Vec::new();
        for row in rows {
            tips.push(row.map_err(sql_err)?);
        }
        Ok(tips)
    }

    fn recent_ids(
        &self,
        object_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, StoreError> {
        let mut sql = String::from(
            "SELECT o.id FROM ref r JOIN object o ON o.sha = r.tip_sha WHERE o.deleted IS NULL",
        );
        let mut args: Vec<String> = Vec::new();
        if let Some(object_type) = object_type {
            sql.push_str(" AND o.type = ?1");
            args.push(object_type.to_string());
        }
        sql.push_str(&format!(
            " ORDER BY o.modified DESC, o.sha ASC LIMIT {limit}"
        ));
        let mut stmt = self.conn.prepare(&sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |row| row.get(0))
            .map_err(sql_err)?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.map_err(sql_err)?);
        }
        Ok(ids)
    }

    fn ids_with_prefix(&self, prefix: &str) -> Result<Vec<IdMatch>, StoreError> {
        // A range on the primary key: every id at or above the prefix
        // and below the prefix followed by a byte above any id char
        let upper = format!("{prefix}~");
        let mut stmt = self
            .conn
            .prepare(
                "SELECT r.id, r.tip_sha, o.type, o.deleted IS NOT NULL \
                 FROM ref r JOIN object o ON o.sha = r.tip_sha \
                 WHERE r.id >= ?1 AND r.id < ?2 ORDER BY r.id",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![prefix, upper], |row| {
                Ok(IdMatch {
                    id: row.get(0)?,
                    tip_sha: row.get(1)?,
                    object_type: row.get(2)?,
                    deleted: row.get(3)?,
                })
            })
            .map_err(sql_err)?;
        let mut matches = Vec::new();
        for row in rows {
            matches.push(row.map_err(sql_err)?);
        }
        Ok(matches)
    }
}

fn sql_err(e: rusqlite::Error) -> StoreError {
    StoreError::Index(e.to_string())
}

fn index_err(message: String) -> StoreError {
    StoreError::Index(message)
}
