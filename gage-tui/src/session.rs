//! Load the session row and entry rows for a session via `gage-query`, plus
//! session-scoped notes from the gage db.

use std::error::Error;

use datafusion::arrow::array::{Array, Int64Array, RecordBatch, StringArray};
use datafusion::arrow::json::ArrayWriter;
use datafusion::prelude::SessionContext;
use gage_db::note::{self, NoteFilters};
use gage_db::rusqlite::Connection;
use serde_json::Value;

use crate::doc::{Document, Entry, Session};

pub async fn load(session_id: &str, db: &Connection) -> Result<Document, Box<dyn Error>> {
    let ctx = gage_query::create_context_default().await;
    let session = load_session(&ctx, session_id).await?;
    let entries = load_entries(&ctx, session_id).await?;
    let notes = note::find(
        db,
        &NoteFilters {
            session: Some(session_id.to_string()),
            ..Default::default()
        },
    )?;
    Ok(Document {
        session,
        entries,
        notes,
    })
}

/// A row in the session-open picker.
pub struct SessionListItem {
    pub id: String,
    /// Encoded project directory name; empty when unrecorded
    pub project: String,
    pub title: String,
    pub mtime_ms: i64,
}

/// Recent sessions from the active corpus, newest first, for the
/// session-open picker.
pub async fn list_recent(limit: usize) -> Result<Vec<SessionListItem>, Box<dyn Error>> {
    use datafusion::arrow::array::TimestampMillisecondArray;

    let ctx = gage_query::create_context_default().await;
    let sql =
        format!("SELECT id, mtime, title, project FROM session ORDER BY mtime DESC LIMIT {limit}");
    let batches = ctx.sql(&sql).await?.collect().await?;
    let mut items = Vec::new();
    for batch in &batches {
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("session.id is a non-null Utf8 column");
        let mtimes = batch
            .column(1)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .expect("session.mtime is a timestamp-ms column");
        let titles = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("session.title is a Utf8 column");
        let projects = batch
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("session.project is a Utf8 column");
        for i in 0..batch.num_rows() {
            items.push(SessionListItem {
                id: ids.value(i).to_string(),
                project: if projects.is_null(i) {
                    String::new()
                } else {
                    projects.value(i).to_string()
                },
                title: if titles.is_null(i) {
                    String::new()
                } else {
                    titles.value(i).to_string()
                },
                mtime_ms: if mtimes.is_null(i) {
                    0
                } else {
                    mtimes.value(i)
                },
            });
        }
    }
    Ok(items)
}

/// Load a session document straight from its JSONL file, bypassing the
/// query index. Used for sessions outside the active corpus (e.g. scan
/// agent sessions opened from `gage scan view`). Session metadata is
/// synthesized from the id and path; notes still come from the gage db.
pub fn load_from_path(
    session_id: &str,
    path: &std::path::Path,
    db: &Connection,
) -> Result<Document, Box<dyn Error>> {
    let mut entries = Vec::new();
    for item in gage_claude::session_reader::SessionReader::open(path)? {
        let (line, value) = item?;
        entries.push(Entry { line, value });
    }
    let notes = note::find(
        db,
        &NoteFilters {
            session: Some(session_id.to_string()),
            ..Default::default()
        },
    )?;
    let session = Session {
        id: session_id.to_string(),
        value: serde_json::json!({
            "id": session_id,
            "path": path.display().to_string(),
        }),
    };
    Ok(Document {
        session,
        entries,
        notes,
    })
}

async fn load_session(ctx: &SessionContext, session_id: &str) -> Result<Session, Box<dyn Error>> {
    let sql = format!(
        "SELECT * FROM session WHERE id = '{}'",
        session_id.replace('\'', "''")
    );
    let batches = ctx.sql(&sql).await?.collect().await?;
    let value = batches
        .iter()
        .find(|b| b.num_rows() > 0)
        .map(first_row_as_value)
        .transpose()?
        .unwrap_or(Value::Null);
    Ok(Session {
        id: session_id.to_string(),
        value,
    })
}

fn first_row_as_value(batch: &RecordBatch) -> Result<Value, Box<dyn Error>> {
    let row = batch.slice(0, 1);
    let mut buf: Vec<u8> = Vec::new();
    let mut writer = ArrayWriter::new(&mut buf);
    writer.write(&row)?;
    writer.finish()?;
    let arr: Vec<Value> = serde_json::from_slice(&buf)?;
    Ok(arr.into_iter().next().unwrap_or(Value::Null))
}

async fn load_entries(
    ctx: &SessionContext,
    session_id: &str,
) -> Result<Vec<Entry>, Box<dyn Error>> {
    let sql = format!(
        "SELECT line, raw FROM entry WHERE session_id = '{}' ORDER BY line",
        session_id.replace('\'', "''")
    );
    let batches = ctx.sql(&sql).await?.collect().await?;
    let mut entries = Vec::new();
    for batch in &batches {
        let lines = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("entry.line is a non-null Int64 column");
        let raws = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("entry.raw is a non-null Utf8 column");
        for i in 0..batch.num_rows() {
            let value = serde_json::from_str(raws.value(i))?;
            let line = u32::try_from(lines.value(i)).unwrap_or(0);
            entries.push(Entry { line, value });
        }
    }
    Ok(entries)
}
