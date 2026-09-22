use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Args, Subcommand};
use cliclack as cli;
use console::style as cstyle;
use datafusion::arrow::array::{
    Array, BooleanArray, Int64Array, StringArray, TimestampMillisecondArray,
};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use gage_claude::home::claude_home;
use gage_claude::session::{encode_project_dir, one_session};
use gage_core::uuid::short_uuid;
use gage_registry::driver::DriverRegistry;
use gage_session::{Driver, Source};
use gage_store::{SessionOutcome, SessionStore, Store};
use gage_tui::{ViewOptions, session_view};
use tabled::{
    Table,
    grid::{config::Position, records::RecordsMut},
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
    },
};

use crate::dialog::{self, DialogError};
use crate::source;
use crate::style;

#[derive(Subcommand)]
pub enum SessionCommand {
    /// List available sessions
    List(SessionListArgs),

    /// Add native sessions to the store
    ///
    /// Each session is read from the selected source and written as a
    /// session object. Adding a session already in the store updates
    /// it when its content changed and is otherwise a no-op.
    Add(SessionAddArgs),

    /// Delete native sessions
    ///
    /// Removes each session's files from the selected source. Stored
    /// sessions are not affected.
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

    /// Filter by project (path or name)
    #[arg(short, long, value_name = "PROJECT", allow_hyphen_values = true)]
    pub project: Option<String>,

    /// Filter by how long ago the session was modified
    #[arg(long, value_parser = super::parse_duration)]
    pub since: Option<Duration>,

    /// Only show empty sessions
    #[arg(long)]
    pub empty: bool,

    /// Show the full session ID
    #[arg(long)]
    pub full_id: bool,
}

#[derive(Args)]
pub struct SessionAddArgs {
    /// Session IDs (or prefixes)
    #[arg(required = true)]
    pub sessions: Vec<String>,
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

pub async fn list(source: Option<String>, stored: bool, args: SessionListArgs) {
    if stored {
        list_stored(args).await
    } else {
        list_native(source, args).await
    }
}

async fn list_native(source: Option<String>, args: SessionListArgs) {
    let registry = source::driver_registry();
    let (driver, spec) = match source::resolve_source(&registry, source.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("gage session list: {e}");
            std::process::exit(1);
        }
    };
    let source = match driver.open_source(&spec) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gage session list: {spec}: {e}");
            std::process::exit(1);
        }
    };
    let ctx = match gage_query::create_source_context(source.as_ref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gage session list: {spec}: {e}");
            std::process::exit(1);
        }
    };
    let project = match args.project.as_deref() {
        Some(text) => match resolve_project(source.as_ref(), text) {
            Ok(name) => Some(name),
            Err(e) => {
                eprintln!("gage session list: {e}");
                std::process::exit(1);
            }
        },
        None => None,
    };
    let (rows, total) = query_sessions(&ctx, &args, Listing::Native, project.as_deref()).await;
    if total > 0 {
        let drivers: Vec<Arc<dyn Driver>> = vec![driver.clone(); rows.len()];
        render_table(&rows, &drivers, Listing::Native, args.full_id);
        args.limit.print_summary(rows.len(), total, "session");
    } else {
        println!("No sessions found");
    }
    if let Err(e) = source.close() {
        eprintln!("gage session list: {spec}: {e}");
        std::process::exit(1);
    }
}

/// List the sessions in the Gage store. The `session` table is bound
/// to the store, so filters and limits are the same SQL as the native
/// listing. Rows are in the store's `modified` order, newest first,
/// and `--since` filters on it; the `Created` column shows when the
/// session was first added.
async fn list_stored(args: SessionListArgs) {
    if args.project.is_some() {
        eprintln!("gage session list: --project does not apply to stored sessions");
        std::process::exit(1);
    }
    if args.empty {
        eprintln!("gage session list: --empty does not apply to stored sessions");
        std::process::exit(1);
    }
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage session list: {e}");
            std::process::exit(1);
        }
    };
    let ctx = gage_query::create_stored_context(Arc::new(Mutex::new(store)));
    let (rows, total) = query_sessions(&ctx, &args, Listing::Stored, None).await;
    if total > 0 {
        let registry = source::driver_registry();
        let drivers = match stored_drivers(&registry, &rows) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("gage session list: {e}");
                std::process::exit(1);
            }
        };
        render_table(&rows, &drivers, Listing::Stored, args.full_id);
        args.limit.print_summary(rows.len(), total, "session");
    } else {
        println!("No sessions found");
    }
}

/// The driver that wrote each stored row, by the name the store
/// recorded. A stored session whose driver this build lacks is an
/// error: its rows cannot be presented.
fn stored_drivers(registry: &DriverRegistry, rows: &[Row]) -> Result<Vec<Arc<dyn Driver>>, String> {
    rows.iter()
        .map(|r| {
            registry
                .for_name(&r.driver_name)
                .ok_or_else(|| format!("{}: unknown session driver '{}'", r.id, r.driver_name))
        })
        .collect()
}

/// Resolve a `--project` value to the driver's project name: an
/// existing directory is named by the driver, anything else is taken
/// as a name.
fn resolve_project(source: &dyn Source, text: &str) -> Result<String, String> {
    let path = Path::new(text);
    if path.is_dir() {
        source
            .project_name(path)
            .map_err(|e| format!("project {text}: {e}"))
    } else {
        Ok(text.to_string())
    }
}

/// Which `session` table binding a listing reads, and therefore
/// which columns it shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Listing {
    /// A native source: Id, Project, Title, Model, Size, Messages,
    /// Modified
    Native,
    /// The store: Id, Project, Title, Type, Model, Size, Messages,
    /// Created
    Stored,
}

/// Run one SQL query for the shown rows and a second for the total
/// count under the same filter. The count is needed for the summary
/// line; DataFusion's `LIMIT` truncates the shown rows and does not
/// report a total. Both bindings select the same column list; the
/// native binding has no type and fills it with an empty literal, and
/// its time column is `mtime` where the store's is `created`.
async fn query_sessions(
    ctx: &SessionContext,
    args: &SessionListArgs,
    listing: Listing,
    project: Option<&str>,
) -> (Vec<Row>, usize) {
    let where_clause = build_where_clause(args, project);
    let limit_clause = match args.limit.fetch_limit() {
        Some(n) => format!(" LIMIT {n}"),
        None => String::new(),
    };
    let (type_col, time_col, driver_col) = match listing {
        Listing::Native => ("'' AS session_type", "mtime AS time", "'' AS driver"),
        Listing::Stored => ("session_type", "created AS time", "driver"),
    };
    let sql = format!(
        "SELECT id, id_display, id_prefix, project, title, {type_col}, model, size, \
         message_count, {time_col}, {driver_col} \
         FROM session{where_clause} \
         ORDER BY mtime DESC{limit_clause}",
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
    (rows, total)
}

fn build_where_clause(args: &SessionListArgs, project: Option<&str>) -> String {
    let mut clauses: Vec<String> = Vec::new();
    if let Some(name) = project {
        clauses.push(format!("project = '{}'", name.replace('\'', "''")));
    }
    if let Some(d) = args.since {
        let cutoff_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_sub(d.as_millis()) as i64;
        clauses.push(format!("mtime >= to_timestamp_millis({cutoff_ms})"));
    }
    if args.empty {
        clauses.push("is_empty".to_string());
    }
    if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    }
}

struct Row {
    id: String,
    /// The short display form of `id`
    id_display: String,
    /// The shortest prefix of `id` unique in its id set
    id_prefix: String,
    /// The driver's project name; empty when the row has none
    project: String,
    title: String,
    /// The session type; empty for a native listing
    session_type: String,
    model: String,
    size: Option<i64>,
    message_count: Option<i64>,
    /// `mtime` for a native listing, `created` for the store
    time_ms: Option<i64>,
    /// The name of the driver that wrote a stored row; empty for a
    /// native listing, whose driver is the one the source was opened
    /// with
    driver_name: String,
}

fn rows_from_batches(batches: &[RecordBatch]) -> Vec<Row> {
    let mut out = Vec::new();
    for batch in batches {
        let ids = column::<StringArray>(batch, 0);
        let id_displays = column::<StringArray>(batch, 1);
        let id_prefixes = column::<StringArray>(batch, 2);
        let projects = column::<StringArray>(batch, 3);
        let titles = column::<StringArray>(batch, 4);
        let types = column::<StringArray>(batch, 5);
        let models = column::<StringArray>(batch, 6);
        let sizes = column::<Int64Array>(batch, 7);
        let counts = column::<Int64Array>(batch, 8);
        let times = column::<TimestampMillisecondArray>(batch, 9);
        let drivers = column::<StringArray>(batch, 10);
        for i in 0..batch.num_rows() {
            out.push(Row {
                id: ids.value(i).to_string(),
                id_display: id_displays.value(i).to_string(),
                id_prefix: id_prefixes.value(i).to_string(),
                project: string_or_empty(projects, i),
                title: string_or_empty(titles, i),
                session_type: string_or_empty(types, i),
                model: string_or_empty(models, i),
                size: sizes.is_valid(i).then(|| sizes.value(i)),
                message_count: counts.is_valid(i).then(|| counts.value(i)),
                time_ms: times.is_valid(i).then(|| times.value(i)),
                driver_name: driver_name_of(drivers.value(i)).to_string(),
            });
        }
    }
    out
}

/// The name part of a stored session's `driver` value, which the
/// store records as `"<name> <version>"`.
fn driver_name_of(driver: &str) -> &str {
    driver.split_once(' ').map_or(driver, |(name, _)| name)
}

fn column<T: 'static>(batch: &RecordBatch, idx: usize) -> &T {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<T>()
        .expect("column type matches session-table schema")
}

fn string_or_empty(col: &StringArray, i: usize) -> String {
    if col.is_null(i) {
        String::new()
    } else {
        col.value(i).to_string()
    }
}

/// Styled id: bright yellow over the unique prefix, dark yellow for
/// the rest of the shown form.
fn styled_id(shown: &str, prefix: &str) -> String {
    let split = shown
        .char_indices()
        .nth(prefix.chars().count())
        .map(|(i, _)| i)
        .unwrap_or(shown.len());
    let (head, tail) = shown.split_at(split);
    format!(
        "{}{}",
        cstyle(head).yellow().bright(),
        cstyle(tail).yellow()
    )
}

/// Print the listing. `drivers` holds the driver of each row in
/// `rows`, which formats the row's model and project names.
///
/// The project cell is seeded with the driver's unbounded form of the
/// name, so the width pass lays out the column against the most it
/// could show. When the pass shrinks the column, each project cell is
/// rewritten with the driver's form for the width the column got.
fn render_table(rows: &[Row], drivers: &[Arc<dyn Driver>], listing: Listing, full_id: bool) {
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    for (r, driver) in rows.iter().zip(drivers) {
        let shown = if full_id { &r.id } else { &r.id_display };
        let id_display = styled_id(shown, &r.id_prefix);
        let project = driver.format_project(&r.project, usize::MAX);
        let time = r
            .time_ms
            .map(crate::human::format_elapsed_ms)
            .unwrap_or_default();
        let size = r.size.map(crate::human::format_size).unwrap_or_default();
        let count = r.message_count.map(|n| n.to_string()).unwrap_or_default();
        let mut cells = vec![id_display, project, r.title.clone()];
        if listing == Listing::Stored {
            cells.push(r.session_type.clone());
        }
        let model = driver.format_model(&r.model);
        cells.extend([model, size, count, time]);
        table_rows.push(cells);
    }

    let header: Vec<String> = match listing {
        Listing::Native => vec![
            "Id", "Project", "Title", "Model", "Size", "Messages", "Modified",
        ],
        Listing::Stored => vec![
            "Id", "Project", "Title", "Type", "Model", "Size", "Messages", "Created",
        ],
    }
    .into_iter()
    .map(String::from)
    .collect();
    let col_count = header.len();
    // Messages is the second column from the right
    let messages_col = col_count - 2;

    let mut table = Table::from_iter(std::iter::once(header).chain(table_rows));
    table
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::new(2..col_count).not(Rows::first()), style::dim())
        .modify(
            Columns::new(messages_col..messages_col + 1),
            Alignment::right(),
        );
    let term_width = console::Term::stdout().size().1 as usize;
    table.with(
        Width::truncate(term_width)
            .suffix("…")
            .priority(style::IdAwarePriority::new(full_id)),
    );
    refit_project_cells(&mut table, rows, drivers);
    println!("{table}");
}

/// Column index of the project cell in the listing
const PROJECT_COL: usize = 1;

/// Rewrite each project cell to the driver's form for the width the
/// width pass gave the column. A table that fit the terminal has no
/// stored widths and keeps its seeded cells.
fn refit_project_cells(table: &mut Table, rows: &[Row], drivers: &[Arc<dyn Driver>]) {
    let Some(width) = table
        .get_dimension()
        .get_widths()
        .and_then(|w| w.get(PROJECT_COL).copied())
    else {
        return;
    };
    let budgets: Vec<usize> = (0..rows.len())
        .map(|i| {
            let pos = Position::new(i + 1, PROJECT_COL);
            let padding = table.get_config().get_padding(pos);
            width.saturating_sub(padding.left.size + padding.right.size)
        })
        .collect();
    let records = table.get_records_mut();
    for (i, ((r, driver), budget)) in rows.iter().zip(drivers).zip(budgets).enumerate() {
        let pos = Position::new(i + 1, PROJECT_COL);
        records.set(pos, driver.format_project(&r.project, budget));
    }
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

pub fn add(source: Option<String>, stored: bool, args: SessionAddArgs) {
    if stored {
        eprintln!("gage session add: --stored does not apply; sessions are added from a source");
        std::process::exit(1);
    }
    let registry = source::driver_registry();
    let (driver, spec) = match source::resolve_source(&registry, source.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("gage session add: {e}");
            std::process::exit(1);
        }
    };
    let source = match driver.open_source(&spec) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gage session add: {spec}: {e}");
            std::process::exit(1);
        }
    };

    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut ids: Vec<String> = Vec::with_capacity(args.sessions.len());
    let mut errors = 0;
    for prefix in &args.sessions {
        match source.find_native(prefix) {
            Ok(id) => ids.push(id),
            Err(e) => {
                eprintln!("gage session add: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage session add: {e}");
            std::process::exit(1);
        }
    };
    let sessions = SessionStore::from(&store);
    for id in &ids {
        let mut session = match source.open_native(id) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("gage session add: {id}: {e}");
                std::process::exit(1);
            }
        };
        let outcome = match sessions.add(driver.as_ref(), session.as_mut()) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("gage session add: {id}: {e}");
                std::process::exit(1);
            }
        };
        let verb = match outcome.outcome {
            SessionOutcome::Added => "Added",
            SessionOutcome::Updated => "Updated",
            SessionOutcome::Unchanged => "Unchanged",
        };
        println!("{verb} {} {id}", short_uuid(&outcome.id));
    }
    if let Err(e) = source.close() {
        eprintln!("gage session add: {spec}: {e}");
        std::process::exit(1);
    }
}

/// The query context over the default source. `delete` and `view`
/// resolve sessions through the pre-driver path and are not yet
/// source-aware; they operate on the default source only.
pub async fn delete(source: Option<String>, stored: bool, args: SessionDeleteArgs) {
    if stored {
        eprintln!(
            "gage session delete: --stored does not apply; sessions are deleted from a source"
        );
        std::process::exit(1);
    }
    if args.ids.is_empty() && !args.empty {
        eprintln!(
            "gage session delete: provide session IDs or --empty\n\n\
            Use 'gage session list' to show sessions"
        );
        std::process::exit(1);
    }

    let registry = source::driver_registry();
    let (driver, spec) = match source::resolve_source(&registry, source.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("gage session delete: {e}");
            std::process::exit(1);
        }
    };
    let source = match driver.open_source(&spec) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gage session delete: {spec}: {e}");
            std::process::exit(1);
        }
    };
    let ctx = match gage_query::create_source_context(source.as_ref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gage session delete: {spec}: {e}");
            std::process::exit(1);
        }
    };

    let targets = if args.empty {
        let spinner = style::spinner("Looking for empty sessions...");
        let targets = delete_targets(&ctx, "SELECT id, is_empty FROM session WHERE is_empty").await;
        spinner.finish_and_clear();
        targets
    } else {
        // Resolve every argument before deleting anything, so one bad
        // argument leaves the source untouched
        let mut ids: Vec<String> = Vec::with_capacity(args.ids.len());
        let mut errors = 0;
        for prefix in &args.ids {
            match source.find_native(prefix) {
                Ok(id) => ids.push(id),
                Err(e) => {
                    eprintln!("gage session delete: {e}");
                    errors += 1;
                }
            }
        }
        if errors > 0 {
            std::process::exit(1);
        }
        let in_list = ids
            .iter()
            .map(|id| format!("'{}'", id.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT id, is_empty FROM session WHERE id IN ({in_list})");
        delete_targets(&ctx, &sql).await
    };
    let empty_count = targets.iter().filter(|t| t.is_empty).count();
    let non_empty_count = targets.len() - empty_count;

    if targets.is_empty() {
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
        for t in &targets {
            if let Err(e) = source.delete_native(&t.id) {
                eprintln!("warning: failed to delete {}: {e}", short_uuid(&t.id));
            } else {
                deleted += 1;
            }
        }

        let plural = if deleted == 1 { "session" } else { "sessions" };
        Ok(format!("Deleted {deleted} {plural}").into())
    });
    if let Err(e) = source.close() {
        eprintln!("gage session delete: {spec}: {e}");
        std::process::exit(1);
    }
}

/// A session selected for deletion
struct DeleteTarget {
    id: String,
    is_empty: bool,
}

/// Run `sql`, which selects `id` and `is_empty`, as the delete targets
async fn delete_targets(ctx: &SessionContext, sql: &str) -> Vec<DeleteTarget> {
    let mut targets = Vec::new();
    for batch in &run_query(ctx, sql).await {
        let ids = column::<StringArray>(batch, 0);
        let empties = column::<BooleanArray>(batch, 1);
        for i in 0..batch.num_rows() {
            targets.push(DeleteTarget {
                id: ids.value(i).to_string(),
                is_empty: empties.is_valid(i) && empties.value(i),
            });
        }
    }
    targets
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
