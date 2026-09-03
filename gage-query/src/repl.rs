use std::sync::Arc;
use std::time::{Duration, Instant};

use datafusion::arrow::array::StringArray;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::physical_plan::{ExecutionPlan, display::DisplayableExecutionPlan, execute_stream};
use datafusion::prelude::SessionContext;
use futures::StreamExt;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::print_format::PrintFormat;
use crate::slow_log;
use crate::tables::{TvfInfo, registered_tvfs};

pub async fn exec_command(
    ctx: &SessionContext,
    sql: &str,
    format: PrintFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    exec_with_stats(ctx, sql, format).await?;
    Ok(())
}

struct QueryStats {
    elapsed: Duration,
    rows: usize,
    batches: usize,
    plan: Arc<dyn ExecutionPlan>,
}

async fn exec_with_stats(
    ctx: &SessionContext,
    sql: &str,
    format: PrintFormat,
) -> Result<QueryStats, Box<dyn std::error::Error>> {
    let start = Instant::now();
    let result = run_query(ctx, sql, format).await;
    let elapsed = start.elapsed();
    match &result {
        Ok((rows, _, _, exec)) => slow_log::record(sql, *exec, Some(*rows), None),
        Err(e) => slow_log::record(sql, elapsed, None, Some(&e.to_string())),
    }
    let (rows, batches, plan, _) = result?;
    Ok(QueryStats {
        elapsed,
        rows,
        batches,
        plan,
    })
}

type QueryOutput = (usize, usize, Arc<dyn ExecutionPlan>, Duration);

async fn run_query(
    ctx: &SessionContext,
    sql: &str,
    format: PrintFormat,
) -> Result<QueryOutput, Box<dyn std::error::Error>> {
    // Time planning and stream-drain only. `print_batch` renders to the
    // terminal and is excluded so the slow log measures execution, not
    // output formatting.
    let exec_start = Instant::now();
    let plan = ctx.sql(sql).await?.create_physical_plan().await?;
    let mut stream = execute_stream(Arc::clone(&plan), ctx.task_ctx())?;
    let mut exec = exec_start.elapsed();
    let mut rows = 0usize;
    let mut batches = 0usize;
    // Buffered formats (Table, Json) need every row before they can
    // emit — one table sized across the whole result, one top-level
    // JSON array spanning every batch. Streaming formats print each
    // batch as it arrives.
    if format.is_buffered() {
        let mut collected: Vec<datafusion::arrow::record_batch::RecordBatch> = Vec::new();
        loop {
            let next_start = Instant::now();
            let next = stream.next().await;
            exec += next_start.elapsed();
            let Some(batch) = next else { break };
            let batch = batch?;
            if batch.num_rows() == 0 {
                continue;
            }
            rows += batch.num_rows();
            batches += 1;
            collected.push(batch);
        }
        format.print_batches(&collected)?;
    } else {
        let mut is_first = true;
        loop {
            let next_start = Instant::now();
            let next = stream.next().await;
            exec += next_start.elapsed();
            let Some(batch) = next else { break };
            let batch = batch?;
            if batch.num_rows() == 0 {
                continue;
            }
            format.print_batch(&batch, is_first)?;
            rows += batch.num_rows();
            batches += 1;
            is_first = false;
        }
    }
    Ok((rows, batches, plan, exec))
}

pub async fn run_repl(
    ctx: &SessionContext,
    index_store: Option<gage_index::IndexStore>,
    mut format: PrintFormat,
    quiet: bool,
    timing: bool,
    stats: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let history_path = gage_core::config::gage_home().join("query_history");
    if let Some(parent) = history_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }

    let mut editor = DefaultEditor::new()?;
    if history_path.exists() {
        editor.load_history(&history_path)?;
    }

    if !quiet {
        println!("gage query - type SQL followed by ; or \\? for help");
    }

    let mut state = ReplState {
        format: &mut format,
        timing,
        stats,
        index_store,
    };
    let mut buf = String::new();

    loop {
        let prompt = if buf.is_empty() { "gage> " } else { "   -> " };

        match editor.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();

                if trimmed.is_empty() {
                    continue;
                }

                if buf.is_empty() && trimmed.starts_with('\\') {
                    editor.add_history_entry(trimmed)?;
                    match handle_backslash(ctx, trimmed, &mut state).await {
                        BackslashResult::Continue => continue,
                        BackslashResult::Quit => break,
                    }
                }

                if !buf.is_empty() {
                    buf.push(' ');
                }
                buf.push_str(trimmed);

                if buf.ends_with(';') {
                    let sql = buf.trim_end_matches(';').trim();
                    if !sql.is_empty() {
                        editor.add_history_entry(&buf)?;
                        match exec_with_stats(ctx, sql, *state.format).await {
                            Ok(stats) => report(&stats, &state),
                            Err(e) => eprintln!("Error: {e}"),
                        }
                    }
                    buf.clear();
                }
            }
            Err(ReadlineError::Interrupted) => {
                buf.clear();
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        }
    }

    editor.save_history(&history_path)?;
    Ok(())
}

struct ReplState<'a> {
    format: &'a mut PrintFormat,
    timing: bool,
    stats: bool,
    index_store: Option<gage_index::IndexStore>,
}

fn report(stats: &QueryStats, state: &ReplState<'_>) {
    if state.timing {
        let ms = stats.elapsed.as_secs_f64() * 1000.0;
        let row_word = if stats.rows == 1 { "row" } else { "rows" };
        let batch_word = if stats.batches == 1 {
            "batch"
        } else {
            "batches"
        };
        println!(
            "Time: {:.3} ms ({} {row_word}, {} {batch_word})",
            ms, stats.rows, stats.batches
        );
    }
    if state.stats {
        let displayable = DisplayableExecutionPlan::with_metrics(stats.plan.as_ref());
        println!("{}", displayable.indent(true));
    }
}

enum BackslashResult {
    Continue,
    Quit,
}

async fn handle_backslash(
    ctx: &SessionContext,
    input: &str,
    state: &mut ReplState<'_>,
) -> BackslashResult {
    let parts: Vec<&str> = input.splitn(2, ' ').collect();
    let cmd = *parts.first().expect("splitn yields at least one substr");
    let arg = parts.get(1).map(|s| s.trim());

    match cmd {
        "\\q" => return BackslashResult::Quit,
        "\\d" => {
            if let Some(table) = arg {
                let sql = format!("DESCRIBE {table}");
                if let Err(e) = exec_command(ctx, &sql, *state.format).await {
                    eprintln!("Error: {e}");
                }
            } else if let Err(e) = exec_command(ctx, "SELECT table_name, table_type FROM information_schema.tables WHERE table_schema = 'public' ORDER BY table_name", *state.format).await {
                eprintln!("Error: {e}");
            }
        }
        "\\df" => {
            if let Err(e) = print_df(arg, *state.format) {
                eprintln!("Error: {e}");
            }
        }
        "\\format" => {
            if let Some(fmt_str) = arg {
                match fmt_str.parse::<PrintFormat>() {
                    Ok(f) => {
                        *state.format = f;
                        println!("Output format: {fmt_str}");
                    }
                    Err(_) => eprintln!("Unknown format: {fmt_str}. Options: table, csv, json, ndjson, yaml"),
                }
            } else {
                eprintln!("Usage: \\format <table|csv|json|ndjson|yaml>");
            }
        }
        "\\timing" => state.timing = parse_toggle(arg, state.timing, "Timing"),
        "\\stats" => state.stats = parse_toggle(arg, state.stats, "Stats"),
        "\\index" => match &state.index_store {
            // The diagnostic for "why didn't my search find X" —
            // one-directional index staleness is otherwise invisible.
            Some(store) => {
                let store = store.clone();
                match tokio::task::spawn_blocking(move || store.status()).await {
                    Ok(status) => println!("{status}"),
                    Err(e) => eprintln!("Error: {e}"),
                }
            }
            None => eprintln!("No index store configured for this session"),
        },
        "\\?" | "\\help" => {
            println!("\\d              List tables");
            println!("\\d <table>      Show table schema");
            println!("\\df             List table-valued functions");
            println!("\\df <function>  Show TVF signature and result columns");
            println!("\\format <fmt>   Set output format (table, csv, json, ndjson, yaml)");
            println!("\\index          Show derived store / text index status");
            println!("\\timing [on|off]  Toggle query wall-clock time");
            println!("\\stats [on|off]   Toggle per-operator plan metrics");
            println!("\\q              Quit");
            println!("\\?              Show this help");
        }
        _ => eprintln!("Unknown command: {cmd}. Try \\? for help"),
    }

    BackslashResult::Continue
}

fn print_df(arg: Option<&str>, format: PrintFormat) -> Result<(), Box<dyn std::error::Error>> {
    let tvfs = registered_tvfs();
    match arg {
        None => {
            let batch = list_tvfs_batch(&tvfs)?;
            format.print_batch(&batch, true)?;
        }
        Some(name) => match tvfs.iter().find(|t| t.name == name) {
            Some(tvf) => {
                let batch = describe_tvf_batch(tvf)?;
                format.print_batch(&batch, true)?;
            }
            None => eprintln!("No table-valued function named: {name}"),
        },
    }
    Ok(())
}

fn list_tvfs_batch(tvfs: &[TvfInfo]) -> Result<RecordBatch, Box<dyn std::error::Error>> {
    let schema: SchemaRef = std::sync::Arc::new(Schema::new(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("arguments", DataType::Utf8, false),
        Field::new("returns", DataType::Utf8, false),
    ]));
    let names: Vec<&str> = tvfs.iter().map(|t| t.name).collect();
    let args: Vec<&str> = tvfs.iter().map(|t| t.args).collect();
    let returns: Vec<String> = tvfs.iter().map(|t| format_returns(&t.schema)).collect();
    let batch = RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(StringArray::from(names)),
            std::sync::Arc::new(StringArray::from(args)),
            std::sync::Arc::new(StringArray::from(returns)),
        ],
    )?;
    Ok(batch)
}

fn describe_tvf_batch(tvf: &TvfInfo) -> Result<RecordBatch, Box<dyn std::error::Error>> {
    let schema: SchemaRef = std::sync::Arc::new(Schema::new(vec![
        Field::new("column_name", DataType::Utf8, false),
        Field::new("data_type", DataType::Utf8, false),
        Field::new("is_nullable", DataType::Utf8, false),
    ]));
    let names: Vec<String> = tvf
        .schema
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    let types: Vec<String> = tvf
        .schema
        .fields()
        .iter()
        .map(|f| format!("{}", f.data_type()))
        .collect();
    let nulls: Vec<&str> = tvf
        .schema
        .fields()
        .iter()
        .map(|f| if f.is_nullable() { "YES" } else { "NO" })
        .collect();
    println!("Function: {}({})", tvf.name, tvf.args);
    let batch = RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(StringArray::from(names)),
            std::sync::Arc::new(StringArray::from(types)),
            std::sync::Arc::new(StringArray::from(nulls)),
        ],
    )?;
    Ok(batch)
}

fn format_returns(schema: &SchemaRef) -> String {
    let cols: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| format!("{} {}", f.name(), f.data_type()))
        .collect();
    format!("TABLE({})", cols.join(", "))
}

fn parse_toggle(arg: Option<&str>, current: bool, label: &str) -> bool {
    let next = match arg {
        None | Some("") => !current,
        Some("on") => true,
        Some("off") => false,
        Some(other) => {
            eprintln!("Unknown value: {other}. Use on, off, or omit to toggle");
            return current;
        }
    };
    println!("{label} is {}", if next { "on" } else { "off" });
    next
}
