//! `gage blame`: the sessions that wrote the lines of a file, read
//! through the `native_blame` and `blame` table functions.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use chrono::{DateTime, Utc};
use clap::Args;
use datafusion::arrow::array::{Array, Int64Array, StringArray, TimestampMillisecondArray};
use datafusion::arrow::record_batch::RecordBatch;
use gage_query::{ContextBuilder, PrintFormat};
use gage_store::Store;
use gage_store::url::LineSelection;

use crate::error::CliError;
use crate::source::{self, SOURCE_HELP};

#[derive(Args)]
pub struct BlameArgs {
    /// File to blame
    path: PathBuf,

    /// Lines to blame (default all)
    ///
    /// A line ("42"), a range ("10-20"), or a comma-separated list of
    /// them ("10-20,42"), 1-based and inclusive: the selection a
    /// session target carries
    #[arg(short, long, value_name = "LINES")]
    lines: Option<LineSelection>,

    /// Session source
    #[arg(short, long, value_name = "SOURCE", long_help = format!("Session source\n\n{SOURCE_HELP}"))]
    source: Option<String>,

    /// Use stored sessions
    #[arg(short = 'S', long, conflicts_with = "source")]
    stored: bool,

    /// Output JSON
    #[arg(long)]
    json: bool,
}

pub async fn run(args: BlameArgs) -> Result<(), CliError> {
    let path = std::fs::canonicalize(&args.path)
        .with_context(|| format!("gage blame: {}", args.path.display()))?;
    let path = path
        .to_str()
        .with_context(|| format!("gage blame: {} is not UTF-8", path.display()))?;
    let lines = args
        .lines
        .map(|l| format!(", {}", quote(&l.to_string())))
        .unwrap_or_default();
    let (ctx, from) = if args.stored {
        let store = Store::open(&gage_store::store_path()).context("gage blame")?;
        let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
            .build()
            .await;
        (ctx, format!("blame({}{lines})", quote(path)))
    } else {
        let registry = source::driver_registry();
        let spec = source::resolve_source(&registry, args.source.as_deref())
            .map_err(|e| anyhow::anyhow!("gage blame: {e}"))?
            .1;
        let ctx = ContextBuilder::new(None).build().await;
        (
            ctx,
            format!("native_blame({}, {}{lines})", quote(&spec), quote(path)),
        )
    };
    let sql = format!(
        "SELECT line, text, session_id, lines, tool_use_id, tool_name, timestamp FROM {from}"
    );
    let batches = ctx
        .sql(&sql)
        .await
        .context("gage blame")?
        .collect()
        .await
        .context("gage blame")?;
    if args.json {
        PrintFormat::Json
            .print_batches(&batches)
            .context("gage blame")?;
    } else {
        print_rows(&batches);
    }
    Ok(())
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// One line per source line: the session prefix, the session lines to
/// read, the call's time, then the source line.
fn print_rows(batches: &[RecordBatch]) {
    let mut rows = Vec::new();
    for batch in batches {
        let numbers = int_col(batch, 0);
        let texts = str_col(batch, 1);
        let sessions = str_col(batch, 2);
        let lines = str_col(batch, 3);
        let timestamps = batch
            .column(6)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .expect("blame projects a millisecond timestamp here");
        for i in 0..batch.num_rows() {
            let attribution = (!sessions.is_null(i)).then(|| {
                (
                    sessions.value(i).chars().take(8).collect::<String>(),
                    lines.value(i).to_string(),
                    format_time(timestamps.value(i)),
                )
            });
            rows.push((numbers.value(i), texts.value(i).to_string(), attribution));
        }
    }
    let width = rows
        .iter()
        .map(|r| r.0)
        .max()
        .unwrap_or(0)
        .to_string()
        .len();
    let lines_width = rows
        .iter()
        .filter_map(|r| r.2.as_ref().map(|a| a.1.len()))
        .max()
        .unwrap_or(1);
    for (number, text, attribution) in &rows {
        match attribution {
            Some((session, lines, time)) => {
                println!("{session} {lines:<lines_width$} ({time} {number:>width$}) {text}")
            }
            None => println!(
                "{:<8} {:<lines_width$} ({:<16} {number:>width$}) {text}",
                "-", "-", "-"
            ),
        }
    }
}

fn format_time(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn str_col(batch: &RecordBatch, i: usize) -> &StringArray {
    batch
        .column(i)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("blame projects a string column here")
}

fn int_col(batch: &RecordBatch, i: usize) -> &Int64Array {
    batch
        .column(i)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("blame projects an integer column here")
}
