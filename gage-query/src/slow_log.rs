use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::Utc;
use rusqlite::Connection;

use gage_core::config::Config;

/// Maximum rows retained in the slow query log. The oldest rows beyond this
/// are pruned on each insert.
const MAX_ROWS: u64 = 100_000;

/// Records a query in the slow query log when it is slow enough or failed.
///
/// `rows` is the row count for a successful query and `None` for a failed
/// one; `error` carries the failure message. Successful queries faster than
/// the configured threshold are not logged. `elapsed` is execution time
/// only — it must not include result rendering. Any database failure is
/// reported via `tracing::warn!` and otherwise ignored so logging never
/// affects query execution.
pub fn record(sql: &str, elapsed: Duration, rows: Option<usize>, error: Option<&str>) {
    let Some(log) = logger() else {
        return;
    };
    if error.is_none() && elapsed < log.threshold {
        return;
    }

    let ms = elapsed.as_secs_f64() * 1000.0;
    let conn = log.conn.lock().expect("slow log connection mutex poisoned");
    if let Err(e) = insert(&conn, sql, ms, rows, error) {
        tracing::warn!("failed to write slow query log: {e}");
    }
}

fn insert(
    conn: &Connection,
    sql: &str,
    ms: f64,
    rows: Option<usize>,
    error: Option<&str>,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO slow_query (ts, query_time_ms, rows, sql, error) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            Utc::now().to_rfc3339(),
            ms,
            rows.map(|n| n as i64),
            sql,
            error
        ],
    )?;
    conn.execute(
        "DELETE FROM slow_query WHERE rowid <= (SELECT MAX(rowid) FROM slow_query) - ?1",
        [MAX_ROWS],
    )?;
    Ok(())
}

struct SlowLog {
    threshold: Duration,
    conn: Mutex<Connection>,
}

fn logger() -> Option<&'static SlowLog> {
    static LOGGER: OnceLock<Option<SlowLog>> = OnceLock::new();
    LOGGER.get_or_init(init).as_ref()
}

fn init() -> Option<SlowLog> {
    let slow_log_ms = match Config::load_user() {
        Ok(cfg) => cfg.query.slow_log_ms,
        Err(e) => {
            tracing::warn!("failed to load config for slow query log: {e}");
            return None;
        }
    };
    if slow_log_ms == 0 {
        return None;
    }

    match open() {
        Ok(conn) => Some(SlowLog {
            threshold: Duration::from_millis(slow_log_ms),
            conn: Mutex::new(conn),
        }),
        Err(e) => {
            tracing::warn!("failed to open slow query log: {e}");
            None
        }
    }
}

/// Opens `<gage_home>/log/slow.db` in WAL mode so that concurrent gage
/// processes (the MCP server and ad-hoc CLI invocations) can write the log
/// without corrupting it — SQLite serializes the writers.
fn open() -> Result<Connection, rusqlite::Error> {
    let path = gage_core::config::gage_home().join("log").join("slow.db");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(e.to_string()),
            )
        })?;
    }
    let conn = Connection::open(&path)?;
    // Set first, before any statement that can contend: without a busy
    // timeout a process that finds the lock held by another fails
    // immediately and its record is dropped. Wait instead so concurrent
    // gage processes serialize rather than lose rows — this covers the
    // WAL switch and table creation below as well as inserts.
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS slow_query (
            ts            TEXT NOT NULL,
            query_time_ms REAL NOT NULL,
            rows          INTEGER,
            sql           TEXT NOT NULL,
            error         TEXT
        )",
    )?;
    Ok(conn)
}
