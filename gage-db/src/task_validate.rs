//! Validation state for keyed task runs. A row records that the task
//! identified by `key` is valid with respect to the entity in `ref`
//! (`session:<id>`, `note:<id>`, or `project:<path>`), optionally
//! qualified by a compared `value` (e.g. a session's size, a project
//! file-set digest). For membership schemes (notes) the row's
//! existence is the state and `value` is NULL.

use rusqlite::{Connection, OptionalExtension, params};

pub fn session_ref(session_id: &str) -> String {
    format!("session:{session_id}")
}

pub fn note_ref(note_id: &str) -> String {
    format!("note:{note_id}")
}

pub fn project_ref(project_path: &str) -> String {
    format!("project:{project_path}")
}

/// Compared value for `(key, ref)`, or `None` when there is no row.
pub fn value(conn: &Connection, key: &str, ref_: &str) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row(
        "SELECT value FROM task_validate WHERE key = ?1 AND ref = ?2",
        params![key, ref_],
        |row| row.get(0),
    )
    .optional()
    .map(Option::flatten)
}

/// Insert or replace the row for `(key, ref)`.
pub fn put(
    conn: &Connection,
    key: &str,
    ref_: &str,
    value: Option<&str>,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR REPLACE INTO task_validate (key, ref, value) VALUES (?1, ?2, ?3)",
        params![key, ref_, value],
    )?;
    Ok(())
}

/// The subset of `refs` that have a row under `key`.
pub fn existing_refs(
    conn: &Connection,
    key: &str,
    refs: &[String],
) -> Result<Vec<String>, rusqlite::Error> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders: Vec<String> = (0..refs.len()).map(|i| format!("?{}", i + 2)).collect();
    let sql = format!(
        "SELECT ref FROM task_validate WHERE key = ?1 AND ref IN ({})",
        placeholders.join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut values: Vec<&dyn rusqlite::types::ToSql> = vec![&key];
    for r in refs {
        values.push(r);
    }
    let found = stmt
        .query_map(values.as_slice(), |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(found)
}

/// Delete the row for `(key, ref)` if present.
pub fn delete(conn: &Connection, key: &str, ref_: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM task_validate WHERE key = ?1 AND ref = ?2",
        params![key, ref_],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_db_in_memory;

    #[test]
    fn value_missing() {
        let conn = open_db_in_memory().unwrap();
        assert_eq!(value(&conn, "k", "session:a").unwrap(), None);
    }

    #[test]
    fn put_value_roundtrip() {
        let conn = open_db_in_memory().unwrap();
        put(&conn, "k", "session:a", Some("100")).unwrap();
        assert_eq!(
            value(&conn, "k", "session:a").unwrap(),
            Some("100".to_string())
        );
    }

    #[test]
    fn put_replaces() {
        let conn = open_db_in_memory().unwrap();
        put(&conn, "k", "session:a", Some("100")).unwrap();
        put(&conn, "k", "session:a", Some("200")).unwrap();
        assert_eq!(
            value(&conn, "k", "session:a").unwrap(),
            Some("200".to_string())
        );
    }

    #[test]
    fn null_value_row_reads_as_none() {
        let conn = open_db_in_memory().unwrap();
        put(&conn, "k", "note:n1", None).unwrap();
        assert_eq!(value(&conn, "k", "note:n1").unwrap(), None);
        // But the row exists for membership
        let found = existing_refs(&conn, "k", &["note:n1".to_string()]).unwrap();
        assert_eq!(found, vec!["note:n1".to_string()]);
    }

    #[test]
    fn delete_removes_only_the_keyed_row() {
        let conn = open_db_in_memory().unwrap();
        put(&conn, "k1", "session:a", Some("100")).unwrap();
        put(&conn, "k2", "session:a", None).unwrap();
        put(&conn, "k1", "session:b", Some("200")).unwrap();

        delete(&conn, "k1", "session:a").unwrap();
        assert_eq!(value(&conn, "k1", "session:a").unwrap(), None);
        assert_eq!(
            value(&conn, "k1", "session:b").unwrap(),
            Some("200".to_string())
        );
        let found = existing_refs(&conn, "k2", &["session:a".to_string()]).unwrap();
        assert_eq!(found, vec!["session:a".to_string()]);

        // Deleting an absent row is not an error
        delete(&conn, "k1", "session:absent").unwrap();
    }

    #[test]
    fn existing_refs_filters_by_key_and_ref() {
        let conn = open_db_in_memory().unwrap();
        put(&conn, "k1", "note:n1", None).unwrap();
        put(&conn, "k1", "note:n2", None).unwrap();
        put(&conn, "k2", "note:n3", None).unwrap();

        let refs = vec![
            "note:n1".to_string(),
            "note:n3".to_string(),
            "note:n4".to_string(),
        ];
        let found = existing_refs(&conn, "k1", &refs).unwrap();
        assert_eq!(found, vec!["note:n1".to_string()]);

        assert!(existing_refs(&conn, "k1", &[]).unwrap().is_empty());
    }
}
