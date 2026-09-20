use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand};
use cliclack as cli;
use datafusion::arrow::array::{
    Array, BooleanArray, Int64Array, StringArray, TimestampMillisecondArray,
};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use gage_claude::home::claude_home;
use gage_claude::session::{delete_session, encode_project_dir, one_session};
use gage_core::uuid::short_uuid;
use gage_tui::{ViewOptions, session_view};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
    },
};

use crate::dialog::{self, DialogError};
use crate::style;

#[derive(Subcommand)]
pub enum SessionCommand {
    /// List available sessions
    List(SessionListArgs),

    /// Delete sessions
    Delete(SessionDeleteArgs),

    /// View a session
    View(SessionViewArgs),

    /// Move a session to a different project directory
    Move(SessionMoveArgs),
}

#[derive(Args)]
pub struct SessionListArgs {
    #[command(flatten)]
    pub limit: crate::limit::LimitArgs,

    /// Filter by project. Accepts a filesystem path or a slug
    /// (e.g. `-home-me-code-foo`).
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<String>,

    /// Filter by how long ago the session was modified (e.g. 1h, 30m, 7d).
    #[arg(long, value_parser = super::parse_duration)]
    pub since: Option<Duration>,

    /// Only show empty sessions.
    #[arg(long)]
    pub empty: bool,

    /// Show the full session ID, never truncating it.
    #[arg(long)]
    pub full_id: bool,
}

#[derive(Args)]
pub struct SessionMoveArgs {
    /// Session ID (or prefix)
    pub session: String,

    /// Destination project directory (must exist)
    pub dir: PathBuf,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct SessionViewArgs {
    /// Session ID (or prefix)
    pub session: Option<String>,

    /// View options (comma-separated)
    ///
    /// Options:
    ///   turns  - show model turns in outline
    ///   detail - show all entries (default hides low-signal entries)
    #[arg(short = 'v', long, value_delimiter = ',')]
    pub options: Vec<String>,
}

#[derive(Args)]
pub struct SessionDeleteArgs {
    /// Session IDs (or prefix)
    #[arg(conflicts_with = "empty")]
    pub ids: Vec<String>,

    /// Delete empty sessions
    #[arg(long)]
    pub empty: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

pub async fn list(source: Option<String>, args: SessionListArgs) {
    if source.is_some() {
        eprintln!("gage session list: --source is not yet wired for SQL contexts");
        std::process::exit(1);
    }
    let ctx = gage_query::create_context_default().await;
    let (rows, total) = match query_sessions(&ctx, &args).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("gage session list: {e}");
            std::process::exit(1);
        }
    };
    if total == 0 {
        println!("No sessions found");
        return;
    }
    render_table(&rows, args.full_id);
    args.limit.print_summary(rows.len(), total, "session");
}

/// Run one SQL query for the shown rows and a second for the total
/// count under the same filter. The count is needed for the summary
/// line; DataFusion's `LIMIT` truncates the shown rows and does not
/// report a total.
async fn query_sessions(
    ctx: &SessionContext,
    args: &SessionListArgs,
) -> Result<(Vec<Row>, usize), String> {
    let where_clause = build_where_clause(args)?;
    let limit = args.limit.show_count(usize::MAX);
    let sql = format!(
        "SELECT id, project, title, model, size, message_count, mtime, is_empty \
         FROM session{where_clause} \
         ORDER BY mtime DESC LIMIT {limit}",
    );
    let batches = run_query(ctx, &sql).await;
    let rows = rows_from_batches(&batches);

    let count_sql = format!("SELECT COUNT(*) FROM session{where_clause}");
    let count_batches = run_query(ctx, &count_sql).await;
    let total = count_batches
        .first()
        .and_then(|b| b.column(0).as_any().downcast_ref::<Int64Array>())
        .map(|a| a.value(0) as usize)
        .unwrap_or(0);
    Ok((rows, total))
}

fn build_where_clause(args: &SessionListArgs) -> Result<String, String> {
    let mut clauses: Vec<String> = Vec::new();
    if let Some(text) = args.project.as_deref() {
        let slug = project_slug(text);
        clauses.push(format!("project = '{}'", slug.replace('\'', "''")));
    }
    if let Some(d) = args.since {
        let cutoff_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_sub(d.as_millis()) as i64;
        clauses.push(format!(
            "mtime >= CAST({cutoff_ms} AS TIMESTAMP) AT TIME ZONE 'UTC'"
        ));
    }
    if args.empty {
        clauses.push("is_empty".to_string());
    }
    if clauses.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!(" WHERE {}", clauses.join(" AND ")))
    }
}

/// Encode a `--project` value to the slug used in the `session.project`
/// column. A filesystem path is encoded through
/// [`encode_project_dir`]; anything else is treated as an already
/// encoded slug.
fn project_slug(text: &str) -> String {
    let looks_like_path = text.starts_with('/')
        || text.starts_with('~')
        || text.starts_with('.')
        || text.contains('/');
    if looks_like_path {
        let path = PathBuf::from(text);
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        encode_project_dir(&canonical)
    } else {
        text.to_string()
    }
}

struct Row {
    id: String,
    project_home_stripped: String,
    title: String,
    model: String,
    size: i64,
    message_count: i64,
    mtime_ms: i64,
    is_empty: bool,
}

fn rows_from_batches(batches: &[RecordBatch]) -> Vec<Row> {
    let mut out = Vec::new();
    let prefix = home_slug_prefix();
    for batch in batches {
        let ids = column::<StringArray>(batch, 0);
        let projects = column::<StringArray>(batch, 1);
        let titles = column::<StringArray>(batch, 2);
        let models = column::<StringArray>(batch, 3);
        let sizes = column::<Int64Array>(batch, 4);
        let counts = column::<Int64Array>(batch, 5);
        let mtimes = column::<TimestampMillisecondArray>(batch, 6);
        let is_empty = column::<BooleanArray>(batch, 7);
        for i in 0..batch.num_rows() {
            let project_name = projects.value(i);
            let stripped = project_name
                .strip_prefix(&prefix)
                .unwrap_or(project_name)
                .to_string();
            let model = if models.is_null(i) {
                String::new()
            } else {
                let m = models.value(i);
                m.strip_prefix("claude-").unwrap_or(m).to_string()
            };
            let title = if titles.is_null(i) {
                String::new()
            } else {
                titles.value(i).to_string()
            };
            out.push(Row {
                id: ids.value(i).to_string(),
                project_home_stripped: stripped,
                title,
                model,
                size: sizes.value(i),
                message_count: counts.value(i),
                mtime_ms: mtimes.value(i),
                is_empty: is_empty.value(i),
            });
        }
    }
    out
}

fn column<T: 'static>(batch: &RecordBatch, idx: usize) -> &T {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<T>()
        .expect("column type matches session-table schema")
}

/// Prefix used to strip the home portion of a Claude project slug so
/// the visible slug is shorter. The old builder computed this the
/// same way from `$HOME`.
fn home_slug_prefix() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut slug = String::with_capacity(home.len() + 1);
    for c in home.chars() {
        slug.push(if c.is_ascii_alphanumeric() { c } else { '-' });
    }
    slug.push('-');
    slug
}

fn render_table(rows: &[Row], full_id: bool) {
    let all_ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    let highlighter = style::IdHighlighter::new(all_ids);

    let mut table_rows: Vec<Vec<String>> = Vec::new();
    for r in rows {
        let id_display = if full_id {
            highlighter.full(&r.id)
        } else {
            highlighter.short(&r.id)
        };
        let modified = crate::human::format_elapsed_ms(r.mtime_ms);
        let size = crate::human::format_size(r.size);
        let count = if r.is_empty && r.message_count == 0 {
            "0".to_string()
        } else {
            r.message_count.to_string()
        };
        table_rows.push(vec![
            id_display,
            r.project_home_stripped.clone(),
            r.title.clone(),
            r.model.clone(),
            size,
            count,
            modified,
        ]);
    }

    let header: Vec<String> = [
        "Id", "Project", "Title", "Model", "Size", "Messages", "Modified",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let col_count = header.len();

    let mut table = Table::from_iter(std::iter::once(header).chain(table_rows));
    table
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::new(2..col_count).not(Rows::first()), style::dim())
        .modify(Columns::new(5..6), Alignment::right());
    let term_width = console::Term::stdout().size().1 as usize;
    table.with(
        Width::truncate(term_width)
            .suffix("…")
            .priority(style::IdAwarePriority::new(full_id)),
    );
    println!("{}", table);
}

async fn run_query(ctx: &SessionContext, sql: &str) -> Vec<RecordBatch> {
    match ctx.sql(sql).await {
        Ok(df) => match df.collect().await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

pub async fn delete(args: SessionDeleteArgs) {
    if args.ids.is_empty() && !args.empty {
        eprintln!(
            "gage session delete: provide session IDs or --empty\n\n\
            Use 'gage session list' to show sessions"
        );
        std::process::exit(1);
    }

    let mut sessions: Vec<(String, PathBuf)> = Vec::new();
    let empty_count;
    let non_empty_count;

    if args.empty {
        let spinner = style::spinner("Looking for empty sessions...");
        let ctx = gage_query::create_context_default().await;
        let sql = "SELECT id, path FROM session WHERE is_empty";
        let batches = run_query(&ctx, sql).await;
        for batch in &batches {
            let ids = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let paths = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            for i in 0..batch.num_rows() {
                sessions.push((ids.value(i).to_string(), PathBuf::from(paths.value(i))));
            }
        }
        spinner.finish_and_clear();
        empty_count = sessions.len();
        non_empty_count = 0;
    } else {
        let ctx = gage_query::create_context_default().await;
        let mut errors = 0;
        for prefix in &args.ids {
            match one_session(prefix) {
                Ok(session) => sessions.push((session.id, session.src)),
                Err(e) => {
                    eprintln!("{e}");
                    errors += 1;
                }
            }
        }
        if errors > 0 {
            std::process::exit(1);
        }

        let in_list = sessions
            .iter()
            .map(|(id, _)| format!("'{}'", id.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT id FROM session WHERE NOT is_empty AND id IN ({in_list})");
        let batches = run_query(&ctx, &sql).await;
        let mut has_messages = std::collections::HashSet::new();
        for batch in &batches {
            let ids = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            for i in 0..batch.num_rows() {
                has_messages.insert(ids.value(i).to_string());
            }
        }
        non_empty_count = sessions
            .iter()
            .filter(|(id, _)| has_messages.contains(id))
            .count();
        empty_count = sessions.len() - non_empty_count;
    }

    if sessions.is_empty() {
        dialog::run("Delete sessions", || Ok("Nothing to delete".into()));
        return;
    }

    dialog::run("Delete sessions", || {
        if empty_count > 0 {
            cli::log::remark(format!("Empty sessions: {empty_count}"))?;
        }
        if non_empty_count > 0 {
            cli::log::remark(format!("Non-empty sessions: {non_empty_count}"))?;
        }

        if !args.yes {
            let confirmed =
                cli::confirm("Permanently delete these sessions? This cannot be undone.")
                    .initial_value(false)
                    .interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let mut deleted = 0;
        for (id, path) in &sessions {
            if let Err(e) = delete_session(path) {
                eprintln!("warning: failed to delete {}: {e}", short_uuid(id));
            } else {
                deleted += 1;
            }
        }

        let plural = if deleted == 1 { "session" } else { "sessions" };
        Ok(format!("Deleted {deleted} {plural}").into())
    });
}

pub async fn view(args: SessionViewArgs) {
    // No session arg: the view opens with its session picker dialog.
    let session_id = match args.session {
        Some(prefix) => match one_session(&prefix) {
            Ok(s) => Some(s.id),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        None => None,
    };
    let options = match ViewOptions::parse(&args.options) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gage session view: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = session_view::run(session_id.as_deref(), options).await {
        eprintln!("gage session view: {e}");
        std::process::exit(1);
    }
}

pub fn move_(args: SessionMoveArgs) {
    let dir = match std::fs::canonicalize(&args.dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("gage session move: {}: {e}", args.dir.display());
            std::process::exit(1);
        }
    };

    let session = match one_session(&args.session) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gage session move: {e}");
            std::process::exit(1);
        }
    };

    let dest_slug = encode_project_dir(&dir);
    if session.project_name() == dest_slug {
        eprintln!("gage session move: session is already in {}", dir.display());
        std::process::exit(1);
    }

    if let Err(e) = check_not_live(&session.id) {
        eprintln!("gage session move: {e}");
        std::process::exit(1);
    }

    let home = claude_home().expect("CLAUDE_CONFIG_DIR or HOME must be set");
    let dest_dir = home.join("projects").join(&dest_slug);
    let dest_jsonl = dest_dir.join(format!("{}.jsonl", session.id));
    let dest_tools = dest_dir.join(&session.id);
    if dest_jsonl.exists() {
        eprintln!(
            "gage session move: destination already has a session with this id: {}",
            dest_jsonl.display()
        );
        std::process::exit(1);
    }

    let src_jsonl = session.src.clone();
    let src_tools = src_jsonl.with_extension("");

    dialog::run("Move session", || {
        cli::log::remark(format!("Session: {}", short_uuid(&session.id)))?;
        cli::log::remark(format!("To: {}", dir.display()))?;

        if !args.yes {
            let confirmed = cli::confirm("Move this session?")
                .initial_value(true)
                .interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        do_move(
            &src_jsonl,
            &src_tools,
            &dest_dir,
            &dest_jsonl,
            &dest_tools,
            &dir,
        )
        .map_err(|e| DialogError::Failed(format!("move failed: {e}")))?;

        Ok(format!(
            "Moved session {} to {}",
            short_uuid(&session.id),
            dir.display()
        )
        .into())
    });
}

fn check_not_live(session_id: &str) -> std::io::Result<()> {
    let home = match claude_home() {
        Some(h) => h,
        None => return Ok(()),
    };
    let sessions_dir = home.join("sessions");
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let value: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("sessionId").and_then(|v| v.as_str()) == Some(session_id) {
            return Err(std::io::Error::other(format!(
                "session is currently live (see {})",
                path.display()
            )));
        }
    }
    Ok(())
}

fn do_move(
    src_jsonl: &std::path::Path,
    src_tools: &std::path::Path,
    dest_dir: &std::path::Path,
    dest_jsonl: &std::path::Path,
    dest_tools: &std::path::Path,
    new_cwd: &std::path::Path,
) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, BufWriter, Write};

    std::fs::create_dir_all(dest_dir)?;
    let tmp = dest_jsonl.with_extension("jsonl.tmp");

    let src = BufReader::new(std::fs::File::open(src_jsonl)?);
    let mut out = BufWriter::new(std::fs::File::create(&tmp)?);
    let new_cwd_str = new_cwd.to_string_lossy();
    for line in src.lines() {
        let line = line?;
        let rewritten = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(mut v) => {
                if let Some(obj) = v.as_object_mut()
                    && obj.get("cwd").is_some_and(|c| c.is_string())
                {
                    obj.insert(
                        "cwd".to_string(),
                        serde_json::Value::String(new_cwd_str.to_string()),
                    );
                    serde_json::to_string(&v).unwrap_or(line)
                } else {
                    line
                }
            }
            Err(_) => line,
        };
        out.write_all(rewritten.as_bytes())?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    drop(out);

    std::fs::rename(&tmp, dest_jsonl)?;
    if src_tools.is_dir() {
        std::fs::rename(src_tools, dest_tools)?;
    }
    std::fs::remove_file(src_jsonl)?;
    Ok(())
}
