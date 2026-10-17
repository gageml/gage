//! `gage blame`: the sessions that wrote the lines of a file, read
//! through the `native_blame` and `blame` table functions.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use chrono::{DateTime, Utc};
use clap::Args;
use datafusion::arrow::array::{Array, Int64Array, StringArray, TimestampMillisecondArray};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use gage_query::{ContextBuilder, PrintFormat};
use gage_store::Store;
use gage_store::url::LineSelection;

use gage_core::style::{IdHighlighter, IdKind};
use gage_core::uuid::short_uuid;

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
        .as_ref()
        .map(|l| format!(", {}", quote(&l.to_string())))
        .unwrap_or_default();
    // `sessions` names the table of every session in the source, the
    // peer set a shown id's unique prefix must distinguish it from
    let (ctx, from, sessions) = if args.stored {
        let store = Store::open(&gage_store::store_path()).context("gage blame")?;
        let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
            .build()
            .await;
        (
            ctx,
            format!("blame({}{lines})", quote(path)),
            "session".to_string(),
        )
    } else {
        let registry = source::driver_registry();
        let spec = source::resolve_source(&registry, args.source.as_deref())
            .map_err(|e| anyhow::anyhow!("gage blame: {e}"))?
            .1;
        let ctx = ContextBuilder::new(None).build().await;
        (
            ctx,
            format!("native_blame({}, {}{lines})", quote(&spec), quote(path)),
            format!("native_session({})", quote(&spec)),
        )
    };
    // JSON emits every candidate; the text form shows the first-ranked
    // attribution per line, in git-blame shape
    let projection = "line, line_end, session_id, session_lines, \
         tool_use_id, tool_name, timestamp, rank, match";
    let sql = if args.json {
        format!("SELECT {projection} FROM {from} ORDER BY line, rank")
    } else {
        format!("SELECT {projection} FROM {from} WHERE rank = 1 ORDER BY line")
    };
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
        let text = std::fs::read_to_string(path).with_context(|| format!("gage blame: {path}"))?;
        let peers = session_ids(&ctx, &sessions).await?;
        print_rows(&batches, &text, args.lines.as_ref(), peers);
    }
    Ok(())
}

/// Every session id in the source, the peer set for prefix
/// disambiguation.
async fn session_ids(ctx: &SessionContext, from: &str) -> Result<Vec<String>, CliError> {
    let batches = ctx
        .sql(&format!("SELECT id FROM {from}"))
        .await
        .context("gage blame")?
        .collect()
        .await
        .context("gage blame")?;
    let mut ids = Vec::new();
    for batch in &batches {
        let col = str_col(batch, 0);
        ids.extend((0..batch.num_rows()).map(|i| col.value(i).to_string()));
    }
    Ok(ids)
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// One line per source line in the selection: the session and lines
/// to read as `<session>:<lines>`, the call's time, then the source
/// line. A line no call wrote prints its number and text with the
/// fields dashed.
fn print_rows(
    batches: &[RecordBatch],
    source: &str,
    selection: Option<&LineSelection>,
    peers: Vec<String>,
) {
    // The attribution of each covered source line: the session id, the
    // session lines to read, and the call time, keyed by line number
    let mut attributed: HashMap<u64, (String, String, String)> = HashMap::new();
    for batch in batches {
        let starts = int_col(batch, 0);
        let ends = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("blame projects line_end as an integer");
        let sessions = str_col(batch, 2);
        let session_lines = str_col(batch, 3);
        let timestamps = batch
            .column(6)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .expect("blame projects a millisecond timestamp here");
        for i in 0..batch.num_rows() {
            let start = starts.value(i) as u64;
            let end = if ends.is_null(i) {
                start
            } else {
                ends.value(i) as u64
            };
            let value = (
                sessions.value(i).to_string(),
                session_lines.value(i).to_string(),
                format_time(timestamps.value(i)),
            );
            for n in start..=end {
                attributed.insert(n, value.clone());
            }
        }
    }

    // The bold span marks the shortest prefix unique among every
    // session in the source, as `gage session list` highlights ids
    let highlighter = IdHighlighter::with_kind(peers, IdKind::Native);
    let locator_width =
        |id: &str, lines: &str| short_uuid(id).chars().count() + 1 + lines.chars().count();

    let lines: Vec<(u64, &str)> = source
        .lines()
        .enumerate()
        .map(|(i, l)| (i as u64 + 1, l))
        .filter(|(n, _)| selection.is_none_or(|s| s.contains(*n)))
        .collect();
    let width = lines
        .iter()
        .map(|(n, _)| *n)
        .max()
        .unwrap_or(0)
        .to_string()
        .len();
    let loc_width = attributed
        .values()
        .map(|(id, lines, _)| locator_width(id, lines))
        .max()
        .unwrap_or(1);
    for (number, text) in lines {
        let num = console::style(format!("{number:>width$}")).dim();
        match attributed.get(&number) {
            Some((id, session_lines, time)) => {
                // Bold the unique prefix inside the yellow locator; the
                // bold span closes without a reset, so yellow carries on
                let body = format!("{}:{session_lines}", highlighter.bold_short(id));
                let locator = console::style(body).yellow();
                let pad = " ".repeat(loc_width.saturating_sub(locator_width(id, session_lines)));
                let time = console::style(format!("{time:<16}")).dim();
                println!("{locator}{pad}  {time}  {num}  {text}")
            }
            None => {
                let time = console::style(format!("{:<16}", "-")).dim();
                println!("{:<loc_width$}  {time}  {num}  {text}", "-")
            }
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
