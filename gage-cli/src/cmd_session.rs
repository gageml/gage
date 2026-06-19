use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand};
use cliclack as cli;
use datafusion::arrow::array::{Array, Int64Array, StringArray, TimestampMillisecondArray};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use gage_claude::home::claude_home;
use gage_claude::session::{delete_session, encode_project_dir};
use gage_core::uuid::short_uuid;
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
        peaker::Peaker,
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
pub struct SessionMoveArgs {
    /// Session ID (prefix match)
    pub session: String,

    /// Destination project directory (must exist)
    pub dir: PathBuf,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct SessionViewArgs {
    /// Session ID (prefix match); omit to pick from recent sessions
    pub session: Option<String>,

    /// View options (comma-separated). Known terms: turns
    #[arg(short, long, value_delimiter = ',')]
    pub options: Vec<String>,
}

#[derive(Args)]
pub struct SessionFilterArgs {
    /// Filter by project path (can be specified multiple times)
    #[arg(short, long, value_name = "PATH")]
    project: Vec<PathBuf>,

    /// Filter to sessions modified within this duration (e.g. 1h, 30m, 7d)
    #[arg(short, long, value_parser = super::parse_duration)]
    since: Option<Duration>,

    /// Only show empty sessions (no message with real content)
    #[arg(long)]
    empty: bool,
}

#[derive(Args)]
pub struct SessionListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,

    #[command(flatten)]
    filter: SessionFilterArgs,

    /// Show the full session ID, never truncating it
    #[arg(long)]
    full_id: bool,

    /// Add per-session stats columns: model time, total tokens, turns.
    /// Computed by parsing each listed session; --stats listings are
    /// slower than the default
    #[arg(long)]
    stats: bool,
}

#[derive(Args)]
pub struct SessionDeleteArgs {
    /// Session IDs (prefix match)
    #[arg(conflicts_with = "empty")]
    pub ids: Vec<String>,

    /// Delete all empty sessions (no message with real content)
    #[arg(long)]
    pub empty: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

fn format_model_time(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs / 60) % 60)
    }
}

fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else if n < 1_000_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    }
}

fn home_slug() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut slug = String::new();
    for c in home.chars() {
        slug.push(if c.is_ascii_alphanumeric() { c } else { '-' });
    }
    slug.push('-');
    slug
}

fn filter_clauses(filter: &SessionFilterArgs) -> Vec<String> {
    let mut clauses = Vec::new();
    for p in &filter.project {
        let path_str = p.to_string_lossy().replace('\'', "''");
        clauses.push(format!("path LIKE '%/{path_str}/%'"));
    }
    if let Some(duration) = filter.since {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64
            - duration.as_micros() as i64;
        clauses.push(format!("mtime >= CAST({cutoff} AS TIMESTAMP)"));
    }
    if filter.empty {
        clauses.push("is_empty".to_string());
    }
    clauses
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

/// Truncates the biggest column first (like `PriorityMax::left`), but never
/// picks the Id column (index 0) when `protect_id` is set, so a full session
/// ID is preserved while the other columns absorb the shrink.
struct IdAwarePriority {
    protect_id: bool,
}

impl IdAwarePriority {
    fn new(protect_id: bool) -> Self {
        Self { protect_id }
    }
}

impl Peaker for IdAwarePriority {
    fn peak(&mut self, mins: &[usize], widths: &[usize]) -> Option<usize> {
        let start = if self.protect_id { 1 } else { 0 };
        widths
            .iter()
            .copied()
            .enumerate()
            .skip(start)
            .rev()
            .filter(|&(i, w)| w != 0 && (mins.is_empty() || mins.get(i).is_none_or(|&m| w > m)))
            .max_by_key(|&(_, w)| w)
            .map(|(i, _)| i)
    }
}

pub async fn list(args: SessionListArgs) {
    let ctx = gage_query::create_context_default().await;

    let clauses = filter_clauses(&args.filter);
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    let count_sql = format!("SELECT COUNT(*) FROM session{where_clause}");
    let count_batches = run_query(&ctx, &count_sql).await;
    let total = count_batches
        .first()
        .map(|b| {
            b.column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(0) as usize
        })
        .unwrap_or(0);
    if total == 0 {
        println!("No sessions found");
        return;
    }

    let show = args.limit.show_count(total);

    let sql = format!(
        "SELECT id, project, mtime, size, title, message_count, path \
         FROM session{where_clause} ORDER BY mtime DESC LIMIT {show}"
    );
    let batches = run_query(&ctx, &sql).await;

    let prefix = home_slug();
    let mut table_rows: Vec<Vec<String>> = Vec::new();

    for batch in &batches {
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let projects = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let mtimes = batch
            .column(2)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .unwrap();
        let sizes = batch
            .column(3)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let titles = batch
            .column(4)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let counts = batch
            .column(5)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let paths = batch
            .column(6)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();

        for i in 0..batch.num_rows() {
            let id = ids.value(i);
            let project_name = projects.value(i);
            let project = project_name
                .strip_prefix(&*prefix)
                .unwrap_or(project_name)
                .to_string();
            let modified = crate::human::format_elapsed_ms(mtimes.value(i));
            let size = crate::human::format_size(sizes.value(i));
            let title = if titles.is_null(i) {
                String::new()
            } else {
                titles.value(i).to_string()
            };
            let count = counts.value(i).to_string();
            let id_display = if args.full_id {
                id.to_string()
            } else {
                short_uuid(id).to_string()
            };
            let mut row = vec![id_display, project, modified, size, title, count];
            if args.stats {
                let (time, tokens, turns) = match gage_claude::stats::compute_session_stats(
                    std::path::Path::new(paths.value(i)),
                ) {
                    Ok(s) => (
                        format_model_time(s.model_time_ms),
                        format_tokens(s.total_tokens),
                        s.turn_count.to_string(),
                    ),
                    Err(_) => ("?".into(), "?".into(), "?".into()),
                };
                row.push(time);
                row.push(tokens);
                row.push(turns);
            }
            table_rows.push(row);
        }
    }

    let mut header: Vec<String> = ["Id", "Project", "Modified", "Size", "Title", "Messages"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if args.stats {
        header.push("Model time".into());
        header.push("Tokens".into());
        header.push("Turns".into());
    }

    let col_count = header.len();
    let mut table = Table::from_iter(std::iter::once(header).chain(table_rows));
    table
        .with(Style::rounded())
        .modify(Rows::first(), Color::FG_BRIGHT_YELLOW)
        .modify(Columns::first().not(Rows::first()), Color::FG_BRIGHT_YELLOW)
        .modify(Columns::new(2..col_count).not(Rows::first()), style::dim())
        .modify(Columns::last(), Alignment::right());
    if args.stats {
        table.modify(Columns::new(6..col_count), Alignment::right());
    }
    let term_width = console::Term::stdout().size().1 as usize;
    table.with(
        Width::truncate(term_width)
            .suffix("…")
            .priority(IdAwarePriority::new(args.full_id)),
    );
    let table = table.to_string();
    println!("{table}");

    args.limit.print_summary(show, total, "session");
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
            match gage_claude::session::one_session(prefix) {
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
    let session_id = match args.session {
        Some(prefix) => match gage_claude::session::one_session(&prefix) {
            Ok(s) => s.id,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        None => match pick_session().await {
            Ok(Some(id)) => id,
            Ok(None) => return,
            Err(e) => {
                eprintln!("gage session view: {e}");
                std::process::exit(1);
            }
        },
    };
    let options = match gage_tui::ViewOptions::parse(&args.options) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gage session view: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = gage_tui::run(&session_id, options).await {
        eprintln!("gage session view: {e}");
        std::process::exit(1);
    }
}

async fn pick_session() -> std::io::Result<Option<String>> {
    let ctx = gage_query::create_context_default().await;
    let sql = "SELECT id, project, mtime, title \
               FROM session ORDER BY mtime DESC LIMIT 30";
    let batches = run_query(&ctx, sql).await;

    let prefix = home_slug();
    let mut items: Vec<(String, String, String)> = Vec::new();
    for batch in &batches {
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let projects = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let mtimes = batch
            .column(2)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .unwrap();
        let titles = batch
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for i in 0..batch.num_rows() {
            let id = ids.value(i).to_string();
            let project = projects
                .value(i)
                .strip_prefix(&*prefix)
                .unwrap_or(projects.value(i))
                .to_string();
            let age = crate::human::format_elapsed_ms(mtimes.value(i));
            let title = if titles.is_null(i) || titles.value(i).is_empty() {
                "(untitled)".to_string()
            } else {
                titles.value(i).to_string()
            };
            let label = format!("{}  {}", short_uuid(&id), title);
            let hint = format!("{project} · {age}");
            items.push((id, label, hint));
        }
    }

    if items.is_empty() {
        println!("No sessions found");
        return Ok(None);
    }

    dialog::install_theme();
    let _sigint = dialog::SigintGuard::new();
    match cli::select("Select a session")
        .items(&items)
        .max_rows(15)
        .filter_mode()
        .interact()
    {
        Ok(id) => Ok(Some(id)),
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(None),
        Err(e) => Err(e),
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

    let session = match gage_claude::session::one_session(&args.session) {
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
