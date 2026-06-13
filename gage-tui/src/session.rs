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
