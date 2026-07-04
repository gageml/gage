use rusqlite::{Connection, OptionalExtension, params};

/// Value for `key`, or `None` when absent or expired. Expired rows are
/// treated as absent; they are overwritten by the next `put`.
pub fn get(conn: &Connection, key: &str) -> Result<Option<String>, rusqlite::Error> {
    let row: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT value, expires FROM cache WHERE key = ?1",
            params![key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(row.and_then(|(value, expires)| match expires {
        Some(t) if t <= gage_core::datetime::now_ms() => None,
        _ => Some(value),
    }))
}

/// Insert or replace the entry for `key`. `expires` is epoch
/// milliseconds; `None` means the entry never expires.
pub fn put(
    conn: &Connection,
    key: &str,
    value: &str,
    expires: Option<i64>,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR REPLACE INTO cache (key, value, expires) VALUES (?1, ?2, ?3)",
        params![key, value, expires],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_db_in_memory;

    #[test]
    fn get_missing() {
        let conn = open_db_in_memory().unwrap();
        assert_eq!(get(&conn, "nope").unwrap(), None);
    }

    #[test]
    fn put_get_roundtrip() {
        let conn = open_db_in_memory().unwrap();
        put(&conn, "k", "v", None).unwrap();
        assert_eq!(get(&conn, "k").unwrap(), Some("v".to_string()));
    }

    #[test]
    fn put_replaces() {
        let conn = open_db_in_memory().unwrap();
        put(&conn, "k", "v1", None).unwrap();
        put(&conn, "k", "v2", None).unwrap();
        assert_eq!(get(&conn, "k").unwrap(), Some("v2".to_string()));
    }

    #[test]
    fn expired_entry_is_absent() {
        let conn = open_db_in_memory().unwrap();
        put(&conn, "k", "v", Some(1)).unwrap();
        assert_eq!(get(&conn, "k").unwrap(), None);
    }

    #[test]
    fn future_expiry_is_present() {
        let conn = open_db_in_memory().unwrap();
        let future = gage_core::datetime::now_ms() + 60_000;
        put(&conn, "k", "v", Some(future)).unwrap();
        assert_eq!(get(&conn, "k").unwrap(), Some("v".to_string()));
    }
}
