use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow};
use clap::{ArgGroup, Args, Subcommand};
use cliclack as cli;
use datafusion::arrow::array::{
    Array, Float32Array, Int64Array, StringArray, TimestampMillisecondArray,
};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use gage_core::uuid::short_uuid;
use gage_query::ContextBuilder;
use gage_registry::driver::DriverRegistry;
use gage_session::{Driver, NativeLookupError, SessionLiveError, Source, SourceOptions};
use gage_store::url::LineSelection;
use gage_store::{
    DatasetStore, PublishStore, SessionRecord, SessionStore, Store, StoreError, TagStore,
    show_publishing,
};
use gage_tui::session::{Backend, picker_page_size};
use gage_tui::{ViewOptions, session_view};
use tabled::{
    Table,
    grid::{config::Position, records::RecordsMut},
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
    },
};
use tokio::task::JoinSet;

use crate::cmd_dataset::{AddEvent, AddView, add_native_to_dataset};
use crate::cmd_tag::TAG_HELP;
use crate::dialog::{self, DialogError, DialogResult};
use crate::error::CliError;
use crate::human::{count, format_elapsed_ms};
use crate::session_filter::{
    FILTER_ARG_NAMES, SessionFilterArgs, SessionLine, native_session_lines, preview_sessions,
    select_sessions,
};
use crate::size_limit::SizeLimitArgs;
use crate::source;
use crate::style::{self, IdKind, styled_id, styled_target};

#[derive(Subcommand)]
pub enum SessionCommand {
    /// List available sessions
    List(SessionListArgs),

    /// Add sessions to store
    ///
    /// Each session is read from the selected source and written as a
    /// session object. Adding a session already in the store updates
    /// it when its content changed.
    Add(SessionAddArgs),

    /// Remove sessions from store or from a dataset
    ///
    /// Without --dataset, each session is unlinked from every dataset
    /// that holds it, then marked removed in the store. With
    /// --dataset, each session is unlinked from that dataset only and
    /// stays in the store. The native session in its source is never
    /// affected.
    Remove(SessionRemoveArgs),

    /// Delete native sessions
    ///
    /// Removes each session's files from the selected source. Stored
    /// sessions are not affected.
    Delete(SessionDeleteArgs),

    /// Move a native session to another project
    ///
    /// Moves the session's files in the selected source to the project
    /// at DIR and records DIR as the session's working directory, so
    /// the harness resumes it from there. Stored sessions are not
    /// affected. A session that is open in the harness is not moved;
    /// --force skips that check.
    Move(SessionMoveArgs),

    /// View a session
    View(SessionViewArgs),

    /// Find sessions by message text
    ///
    /// Searches message text in the default source, in each --source,
    /// and in the store with --stored, and lists the matching messages
    /// best match first. Bare terms match any of them; "a AND b",
    /// "+a", "-a", a quoted phrase, and a field term such as
    /// "type:assistant" narrow a search. --all shows up to 1000
    /// matches per source
    Find(SessionFindArgs),
}

#[derive(Args)]
#[command(next_display_order = 3)]
pub struct SessionListArgs {
    /// Filter by project (path or name)
    #[arg(short, long, value_name = "PROJECT", allow_hyphen_values = true)]
    pub project: Option<String>,

    /// Show sessions modified within SINCE
    #[arg(long, value_parser = super::parse_duration)]
    pub since: Option<Duration>,

    /// Only show empty sessions
    #[arg(long)]
    pub empty: bool,

    /// List sessions in dataset (ID or prefix)
    ///
    /// Dataset ID (or prefix). Implies --stored; rows are in dataset
    /// member order
    #[arg(short, long, value_name = "DATASET")]
    pub dataset: Option<String>,

    #[command(flatten)]
    pub limit: crate::limit::LimitArgs,

    /// Show full session ID
    #[arg(long)]
    pub full_id: bool,
}

#[derive(Args)]
#[command(next_display_order = 3)]
#[command(group = ArgGroup::new("session_add_select")
    .required(true)
    .multiple(true)
    .arg("sessions")
    .args(FILTER_ARG_NAMES))]
pub struct SessionAddArgs {
    /// Sessions to add (ID or prefix)
    #[arg(value_name = "SESSION", conflicts_with_all = FILTER_ARG_NAMES)]
    pub sessions: Vec<String>,

    /// Add sessions to dataset (ID or prefix)
    ///
    /// Dataset ID (or prefix). Native sessions are added to the store
    /// and linked in one step; with --stored, sessions already in the
    /// store are linked
    #[arg(short, long, value_name = "DATASET")]
    pub dataset: Option<String>,

    #[command(flatten)]
    pub filter: SessionFilterArgs,

    #[command(flatten)]
    pub size: SizeLimitArgs,

    /// Tag added session
    #[arg(
        short,
        long,
        value_name = "NAME",
        long_help = format!(
            "Tag added session\n\nApplies when one session is given. The tag names the \
             stored session, whether it is added from a source or linked with --stored.\n\n\
             {TAG_HELP}"
        )
    )]
    pub tag: Option<String>,

    /// Move tag if it exists
    #[arg(long, requires = "tag")]
    pub force_tag: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Args)]
#[command(next_display_order = 3)]
pub struct SessionRemoveArgs {
    /// Stored sessions to remove (ID or prefix)
    #[arg(required = true)]
    pub ids: Vec<String>,

    /// Remove sessions from dataset only (ID or prefix)
    ///
    /// Dataset ID (or prefix). The sessions stay in the store
    #[arg(short, long, value_name = "DATASET")]
    pub dataset: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Args)]
#[command(next_display_order = 3)]
pub struct SessionViewArgs {
    /// Session to view (ID or prefix)
    ///
    /// A trailing ":LINES" opens the view at a session line, as
    /// "7260:2956". LINES is a line ("42"), a range ("10-20"), or a
    /// comma-separated list of them ("10-20,42"), the selection a
    /// session target carries; the view opens at the first line
    pub session: Option<String>,

    /// View options
    ///
    /// Comma separated list of the following values:
    ///
    /// Entry types in outline:
    ///   a, assistant  assistant messages and thinking (default on)
    ///   u, user       user messages (default on)
    ///   t, tools      tool calls (default off)
    ///   e, entries    all entries, including system and bookkeeping
    ///
    /// Content and extras:
    ///   s, source     native source in content pane
    ///   n, turns      turn markers in outline
    ///   r, redacted   redactions of a published session
    ///
    /// A leading "!" turns an option off, as in "!user"
    #[arg(short = 'v', long, verbatim_doc_comment)]
    pub options: Option<String>,
}

#[derive(Args)]
#[command(next_display_order = 3)]
pub struct SessionFindArgs {
    /// Text to find
    pub query: String,

    /// Session sources to search (repeatable)
    #[arg(
        id = "source",
        short = 's',
        long = "source",
        value_name = "SOURCE",
        action = clap::ArgAction::Append,
        long_help = format!("Session sources to search (repeatable)\n\n{}", source::SOURCE_HELP),
        display_order = 1
    )]
    pub sources: Vec<String>,

    /// Search stored sessions
    #[arg(id = "stored", short = 'S', long = "stored", display_order = 2)]
    pub stored: bool,

    /// Sort hits by date, most recent first
    ///
    /// Sort hits by date, most recent first. The hits shown are
    /// still the best matches; this option changes their order only.
    #[arg(short = 'D', long)]
    pub by_date: bool,

    #[command(flatten)]
    pub limit: crate::limit::LimitArgs,

    /// Show full session ID
    #[arg(long)]
    pub full_id: bool,
}

#[derive(Args)]
#[command(next_display_order = 3)]
pub struct SessionDeleteArgs {
    /// Sessions to delete (ID or prefix)
    #[arg(conflicts_with = "empty")]
    pub ids: Vec<String>,

    /// Delete empty sessions
    #[arg(long)]
    pub empty: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Args)]
#[command(next_display_order = 3)]
pub struct SessionMoveArgs {
    /// Session to move (ID or prefix)
    pub session: String,

    /// Project directory to move session to
    pub dir: PathBuf,

    /// Move session without checking that it is closed
    #[arg(long)]
    pub force: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

pub async fn list(
    source: Option<String>,
    stored: bool,
    args: SessionListArgs,
) -> Result<(), CliError> {
    if args.dataset.is_some() {
        if source.is_some() {
            return Err(CliError::message(
                "gage session list: --source does not apply with --dataset; a dataset holds stored sessions",
            ));
        }
        if stored {
            eprintln!("warning: --stored is redundant with --dataset");
        }
        list_dataset(args).await
    } else if stored {
        list_stored(args).await
    } else {
        list_native(source, args).await
    }
}

async fn list_native(source: Option<String>, args: SessionListArgs) -> Result<(), CliError> {
    let registry = source::driver_registry();
    let (driver, spec) = source::resolve_source(&registry, source.as_deref())
        .map_err(|e| anyhow!("gage session list: {e}"))?;
    // The native session rows, the project filter, and the driver
    // encoding all reach the source through SQL: `native_session`
    // opens it, `project_for_path` resolves a directory to its project
    // name. The command holds no source handle.
    let ctx = ContextBuilder::new(None).build().await;
    let from = format!("native_session('{}')", spec.replace('\'', "''"));
    let project_pred = args
        .project
        .as_deref()
        .map(|p| format!("project = project_for_path('{}')", p.replace('\'', "''")));
    let (rows, total) =
        query_sessions(&ctx, &args, Listing::Native, &from, project_pred.as_deref())
            .await
            .context("gage session list")?;
    if total > 0 {
        let drivers: Vec<Arc<dyn Driver>> = vec![driver.clone(); rows.len()];
        render_table(&rows, &drivers, Listing::Native, args.full_id);
        args.limit.print_summary(rows.len(), total, "session");
    } else {
        println!("No sessions found");
    }
    Ok(())
}

/// List the sessions in the Gage store. The `session` table is bound
/// to the store, so filters and limits are the same SQL as the native
/// listing. Rows are in the store's `modified` order, newest first,
/// and `--since` filters on it; the `Created` column shows when the
/// session was first added.
async fn list_stored(args: SessionListArgs) -> Result<(), CliError> {
    if args.project.is_some() {
        return Err(CliError::message(
            "gage session list: --project does not apply to stored sessions",
        ));
    }
    let store = Store::open(&gage_store::store_path()).context("gage session list")?;
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let (rows, total) = query_sessions(&ctx, &args, Listing::Stored, "session", None)
        .await
        .context("gage session list")?;
    if total > 0 {
        let registry = source::driver_registry();
        let drivers = stored_drivers(&registry, &rows).context("gage session list")?;
        render_table(&rows, &drivers, Listing::Stored, args.full_id);
        args.limit.print_summary(rows.len(), total, "session");
    } else {
        println!("No sessions found");
    }
    Ok(())
}

/// List the member sessions of a dataset, each at the commit the
/// dataset links, in member order: the `session` table scoped to the
/// dataset, joined to `dataset_session` for the order.
async fn list_dataset(args: SessionListArgs) -> Result<(), CliError> {
    if args.project.is_some() {
        return Err(CliError::message(
            "gage session list: --project does not apply to stored sessions",
        ));
    }
    let prefix = args
        .dataset
        .as_deref()
        .expect("list_dataset is called with --dataset");
    let store = Store::open(&gage_store::store_path()).context("gage session list")?;
    let dataset = DatasetStore::from(&store)
        .get(prefix)
        .with_context(|| format!("gage session list: --dataset {prefix}"))?;
    let store = Arc::new(Mutex::new(store));
    let ctx = ContextBuilder::new(Some(store)).build().await;
    let from = "session JOIN dataset_session m ON m.session_id = session.id";
    let member = format!("m.dataset_id = '{}'", dataset.id);
    let (rows, total) = query_sessions(&ctx, &args, Listing::Dataset, from, Some(&member))
        .await
        .context("gage session list")?;
    if total == 0 {
        println!("No sessions found");
        return Ok(());
    }
    let registry = source::driver_registry();
    let drivers = stored_drivers(&registry, &rows).context("gage session list")?;
    render_table(&rows, &drivers, Listing::Dataset, args.full_id);
    args.limit.print_summary(rows.len(), total, "session");
    Ok(())
}

fn stored_drivers(registry: &DriverRegistry, rows: &[Row]) -> anyhow::Result<Vec<Arc<dyn Driver>>> {
    rows.iter()
        .map(|r| {
            registry
                .for_name(&r.driver_name)
                .ok_or_else(|| anyhow!("{}: unknown session driver '{}'", r.id, r.driver_name))
        })
        .collect()
}

/// Resolve a `--project` value to the driver's project name: an
/// existing directory is named by the driver, anything else is taken
/// as a name.
/// Which `session` table binding a listing reads, and therefore
/// which columns it shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Listing {
    /// A native source: Native Id, Project, Title, Model, Size,
    /// Messages, Modified
    Native,
    /// The store: Id, Project, Title, Type, Model, Size, Messages,
    /// Created
    Stored,
    /// A dataset's members, the stored columns in member order
    Dataset,
}

impl Listing {
    /// The column `--since` filters on: the native mtime for a
    /// source, the store's `modified` marker for the store
    fn since_col(self) -> &'static str {
        match self {
            Listing::Native => "mtime",
            Listing::Stored | Listing::Dataset => "modified",
        }
    }

    /// The `ORDER BY` clause: newest first for a source and the store,
    /// member order for a dataset
    fn order_clause(self) -> &'static str {
        match self {
            Listing::Native => "mtime DESC",
            Listing::Stored => "modified DESC",
            Listing::Dataset => "m.session_num ASC",
        }
    }
}

/// Run one SQL query for the shown rows and a second for the total
/// count under the same filter. The count is needed for the summary
/// line; DataFusion's `LIMIT` truncates the shown rows and does not
/// report a total. Both bindings select the same column list; the
/// native binding has no type and fills it with an empty literal, and
/// reads `title`, `model`, and `message_count` as its own columns
/// where the store reads them from its `summary` JSON; its time
/// column is `mtime` where the store's is `created`; and rows order
/// and `--since` filter on `mtime` where the store's do on
/// `modified`.
async fn query_sessions(
    ctx: &SessionContext,
    args: &SessionListArgs,
    listing: Listing,
    from: &str,
    project_pred: Option<&str>,
) -> datafusion::error::Result<(Vec<Row>, usize)> {
    let where_clause = build_where_clause(args, listing, project_pred);
    let limit_clause = match args.limit.fetch_limit() {
        Some(n) => format!(" LIMIT {n}"),
        None => String::new(),
    };
    let (type_col, title_col, model_col, count_col, time_col, driver_col) = match listing {
        Listing::Native => (
            "'' AS session_type",
            "title",
            "model",
            "message_count",
            "mtime AS time",
            "'' AS driver",
        ),
        Listing::Stored | Listing::Dataset => (
            "session_type",
            "json_extract(summary, '$.title') AS title",
            "json_extract(summary, '$.model') AS model",
            "CAST(json_extract(summary, '$.message_count') AS BIGINT) AS message_count",
            "created AS time",
            "driver",
        ),
    };
    let order_clause = listing.order_clause();
    let sql = format!(
        "SELECT id, id_display, id_prefix, project, {title_col}, {type_col}, {model_col}, size, \
         {count_col}, {time_col}, {driver_col} \
         FROM {from}{where_clause} \
         ORDER BY {order_clause}{limit_clause}",
    );
    let batches = run_query(ctx, &sql).await?;
    let rows = rows_from_batches(&batches);

    let count_sql = format!("SELECT COUNT(*) FROM {from}{where_clause}");
    let count_batches = run_query(ctx, &count_sql).await?;
    let total = count_batches
        .first()
        .and_then(|b| b.column(0).as_any().downcast_ref::<Int64Array>())
        .map(|a| a.value(0) as usize)
        .unwrap_or(0);
    Ok((rows, total))
}

fn build_where_clause(
    args: &SessionListArgs,
    listing: Listing,
    project_pred: Option<&str>,
) -> String {
    let mut clauses: Vec<String> = Vec::new();
    if let Some(pred) = project_pred {
        clauses.push(pred.to_string());
    }
    if let Some(d) = args.since {
        let cutoff_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_sub(d.as_millis()) as i64;
        clauses.push(format!(
            "{} >= to_timestamp_millis({cutoff_ms})",
            listing.since_col()
        ));
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

pub(crate) fn column<T: 'static>(batch: &RecordBatch, idx: usize) -> &T {
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

/// Print the listing. `drivers` holds the driver of each row in
/// `rows`, which formats the row's model and project names.
///
/// The project cell is seeded with the driver's unbounded form of the
/// name, so the width pass lays out the column against the most it
/// could show. When the pass shrinks the column, each project cell is
/// rewritten with the driver's form for the width the column got.
fn render_table(rows: &[Row], drivers: &[Arc<dyn Driver>], listing: Listing, full_id: bool) {
    let id_kind = match listing {
        Listing::Native => IdKind::Native,
        Listing::Stored | Listing::Dataset => IdKind::Gage,
    };
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    for (r, driver) in rows.iter().zip(drivers) {
        let shown = if full_id { &r.id } else { &r.id_display };
        let id_display = styled_id(shown, &r.id_prefix, id_kind);
        let project = driver.format_project(&r.project, usize::MAX);
        let time = r
            .time_ms
            .map(crate::human::format_elapsed_ms)
            .unwrap_or_default();
        let size = r.size.map(crate::human::format_size).unwrap_or_default();
        let count = r.message_count.map(|n| n.to_string()).unwrap_or_default();
        let mut cells = vec![id_display, project, r.title.clone()];
        if listing != Listing::Native {
            cells.push(r.session_type.clone());
        }
        let model = driver.format_model(&r.model);
        cells.extend([model, size, count, time]);
        table_rows.push(cells);
    }

    let header: Vec<String> = match listing {
        Listing::Native => vec![
            "Native Id",
            "Project",
            "Title",
            "Model",
            "Size",
            "Messages",
            "Modified",
        ],
        Listing::Stored | Listing::Dataset => vec![
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
            .priority(style::IdAwarePriority::new(full_id).weight(TITLE_COL, 2)),
    );
    refit_project_cells(&mut table, rows, drivers);
    println!("{table}");
}

/// Column index of the project cell in the listing
const PROJECT_COL: usize = 1;

/// Column index of the title cell in the listing
const TITLE_COL: usize = 2;

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

/// Run `sql` and collect its batches
pub(crate) async fn run_query(
    ctx: &SessionContext,
    sql: &str,
) -> datafusion::error::Result<Vec<RecordBatch>> {
    ctx.sql(sql).await?.collect().await
}

pub async fn add(
    source: Option<String>,
    stored: bool,
    args: SessionAddArgs,
) -> Result<(), CliError> {
    if stored && args.dataset.is_none() {
        return Err(CliError::message(
            "gage session add: --stored applies only with --dataset; sessions are added from a source",
        ));
    }
    // --stored --dataset resolves stored session IDs, not the native
    // filter axes. Reject anything but positional session IDs here so
    // a filter given with --stored fails up front.
    if stored && (args.sessions.is_empty() || !args.filter.is_empty()) {
        return Err(CliError::message(
            "gage session add: --stored --dataset takes stored session IDs; the selection flags apply to native sessions",
        ));
    }
    if args.tag.is_some() && !selects_one_session(&args) {
        return Err(CliError::message(
            "gage session add: --tag applies to a single session; give one session ID and no selection flags",
        ));
    }
    dialog::run_async("Add sessions", || add_dialog(source, stored, args)).await
}

/// The selection is one positional session ID with no filter flags
fn selects_one_session(args: &SessionAddArgs) -> bool {
    args.sessions.len() == 1 && args.filter.is_empty()
}

async fn add_dialog(
    source: Option<String>,
    stored: bool,
    args: SessionAddArgs,
) -> Result<dialog::DialogResult, DialogError> {
    let store =
        Store::open(&gage_store::store_path()).map_err(|e| DialogError::Failed(format!("{e}")))?;
    let dataset_id = match args.dataset.as_deref() {
        Some(prefix) => Some(
            DatasetStore::from(&store)
                .resolve_id(prefix)
                .map_err(|e| DialogError::Failed(format!("--dataset {prefix}: {e}")))?,
        ),
        None => None,
    };
    let max_bytes = Some(
        args.size
            .max_bytes()
            .map_err(|e| DialogError::Other(e.into()))?,
    );
    let tags = TagStore::from(&store);
    // A tag that cannot be added is reported before anything is
    // written, so the failure leaves the store untouched
    if let Some(name) = &args.tag {
        tags.check_available(name, args.force_tag)
            .map_err(|e| match e {
                StoreError::TagExists { .. } => {
                    DialogError::Failed(format!("{e} (use --force-tag to move it)"))
                }
                e => DialogError::Failed(e.to_string()),
            })?;
    }
    let mut view = AddView::new();
    let placed_ids = match dataset_id {
        Some(dataset_id) if stored => {
            let sessions = SessionStore::from(&store);
            // Resolve every argument before writing anything, so one
            // bad argument leaves the store untouched
            let mut records: Vec<SessionRecord> = Vec::with_capacity(args.sessions.len());
            let mut errors = Vec::new();
            for prefix in &args.sessions {
                match sessions.get(prefix) {
                    Ok(record) => records.push(record),
                    Err(e) => errors.push(e.to_string()),
                }
            }
            if !errors.is_empty() {
                return Err(DialogError::Failed(errors.join("\n")));
            }
            dialog::step_value("Dataset", short_uuid(&dataset_id))?;
            let lines: Vec<SessionLine> = records.iter().map(SessionLine::from).collect();
            preview_sessions(&lines, IdKind::Gage, args.tag.as_deref())?;
            confirm_add(args.yes)?;
            link_stored(&store, &dataset_id, &records, &mut |e| view.on(e))?;
            records.into_iter().map(|r| r.id).collect()
        }
        _ => {
            let registry = source::driver_registry();
            let (driver, spec) = source::resolve_source(&registry, source.as_deref())
                .map_err(DialogError::Failed)?;
            let selected = select_sessions(source.as_deref(), &args.sessions, &args.filter)
                .await
                .map_err(|e| DialogError::Failed(format!("{e}")))?;
            if selected.is_empty() {
                return Err(DialogError::Failed(
                    "No sessions matched the selection".to_string(),
                ));
            }
            dialog::step_value("Source", source::describe_source(driver.as_ref(), &spec))?;
            if let Some(id) = &dataset_id {
                dialog::step_value("Dataset", short_uuid(id))?;
            }
            let lines = native_session_lines(source.as_deref(), &selected)
                .await
                .map_err(|e| DialogError::Failed(format!("{e}")))?;
            preview_sessions(&lines, IdKind::Native, args.tag.as_deref())?;
            confirm_add(args.yes)?;
            add_native(
                &store,
                source,
                dataset_id.as_deref(),
                &selected,
                max_bytes,
                &mut |e| view.on(e),
            )?
        }
    };
    let done = view.done().to_string();
    view.finish()?;
    if let Some(name) = &args.tag {
        // The selection check in `add` admits one session, and an
        // interrupted add of one session fails before this point
        let id = placed_ids
            .first()
            .expect("a tagged add should place one session");
        let added = tags
            .add(name, id, args.force_tag)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        match added.previous_id {
            Some(previous) => cli::log::info(format!(
                "Moved tag {name} from {} to session {}",
                short_uuid(&previous),
                short_uuid(id)
            ))?,
            None => cli::log::info(format!("Tagged session {} as {name}", short_uuid(id)))?,
        }
    }
    Ok(done.into())
}

/// The confirmation before an add writes; `yes` skips it
fn confirm_add(yes: bool) -> Result<(), DialogError> {
    if yes {
        return Ok(());
    }
    if !cli::confirm("Continue?").initial_value(true).interact()? {
        return Err(DialogError::Canceled);
    }
    Ok(())
}

/// Add every session in `source` to `store` inside the caller's
/// dialog, rendering progress as `gage session add --all` does.
/// Returns the closing line for the caller to log.
pub(crate) async fn add_all(
    store: &Store,
    source: &str,
    max_bytes: u64,
) -> Result<String, DialogError> {
    let filter = SessionFilterArgs {
        project: None,
        days: None,
        today: false,
        all: true,
        limit: None,
        sample: None,
    };
    let selected = select_sessions(Some(source), &[], &filter)
        .await
        .map_err(|e| DialogError::Failed(format!("{source}: {e}")))?;
    if selected.is_empty() {
        return Ok(format!("No sessions in {source}"));
    }
    let mut view = AddView::new();
    add_native(
        store,
        Some(source.to_string()),
        None,
        &selected,
        Some(max_bytes),
        &mut |e| view.on(e),
    )?;
    let done = view.done().to_string();
    view.finish()?;
    Ok(format!("{done} from {source}"))
}

/// Link sessions already in the store as members of `dataset_id`,
/// reporting each to `on_event`.
fn link_stored(
    store: &Store,
    dataset_id: &str,
    records: &[SessionRecord],
    on_event: &mut dyn FnMut(AddEvent<'_>),
) -> Result<(), DialogError> {
    let ids: Vec<String> = records.iter().map(|r| r.id.clone()).collect();
    let outcomes = DatasetStore::from(store)
        .sessions_link(dataset_id, &ids)
        .map_err(|e| DialogError::Failed(format!("{e}")))?;
    for (outcome, record) in outcomes.iter().zip(records) {
        on_event(AddEvent::Added {
            outcome: outcome.outcome,
            id: &outcome.id,
            native_id: record.attrs.native_id(),
            dataset: Some(dataset_id),
        });
    }
    on_event(AddEvent::Done {
        placed: outcomes.len(),
        dataset: Some(dataset_id),
    });
    Ok(())
}

/// Add native sessions from `source` to the store and, when
/// `dataset_id` is given, link them as members in the same operation,
/// reporting each to `on_event`. `selected` holds native ids resolved
/// by the shared session picker; they are re-checked against the
/// opened source before anything is written. Returns the stored id of
/// each placed session, in `selected` order.
fn add_native(
    store: &Store,
    source: Option<String>,
    dataset_id: Option<&str>,
    selected: &[String],
    max_bytes: Option<u64>,
    on_event: &mut dyn FnMut(AddEvent<'_>),
) -> Result<Vec<String>, DialogError> {
    if let Some(dataset_id) = dataset_id {
        let outcomes = add_native_to_dataset(
            store,
            dataset_id,
            source.as_deref(),
            selected,
            max_bytes,
            on_event,
        )
        .map_err(DialogError::Failed)?;
        return Ok(outcomes.into_iter().map(|o| o.id).collect());
    }
    let registry = source::driver_registry();
    let (driver, spec) =
        source::resolve_source(&registry, source.as_deref()).map_err(DialogError::Failed)?;
    let opened = driver
        .open_source(&SourceOptions::new(&spec))
        .map_err(|e| DialogError::Failed(format!("{spec}: {e}")))?;

    // Confirm every selected session is present in the opened source
    // before writing anything, so one missing session leaves the
    // store untouched.
    let mut ids: Vec<String> = Vec::with_capacity(selected.len());
    let mut errors = Vec::new();
    for native_id in selected {
        match opened.find_native(native_id) {
            Ok(id) => ids.push(id),
            Err(e) => errors.push(e.to_string()),
        }
    }
    if !errors.is_empty() {
        return Err(DialogError::Failed(errors.join("\n")));
    }

    let sessions = match max_bytes {
        Some(max) => SessionStore::from(store).with_max_session_size(max),
        None => SessionStore::from(store),
    };
    let total = ids.len();
    let mut placed: Vec<String> = Vec::with_capacity(total);
    for id in &ids {
        if dialog::interrupted() {
            break;
        }
        on_event(AddEvent::Starting {
            total,
            native_id: id,
        });
        let mut session = opened
            .open_native(id)
            .map_err(|e| DialogError::Failed(format!("{id}: {e}")))?;
        let outcome = sessions
            .add(driver.as_ref(), session.as_mut())
            .map_err(|e| DialogError::Failed(format!("{id}: {e}")))?;
        on_event(AddEvent::Added {
            outcome: outcome.outcome,
            id: &outcome.id,
            native_id: id,
            dataset: None,
        });
        placed.push(outcome.id);
    }
    on_event(AddEvent::Done {
        placed: placed.len(),
        dataset: None,
    });
    opened
        .close()
        .map_err(|e| DialogError::Failed(format!("{spec}: {e}")))?;
    if placed.len() < total {
        return Err(dialog::partial(
            "Interrupted after",
            placed.len(),
            total,
            "session",
        ));
    }
    Ok(placed)
}

/// Matches per corpus when every match is wanted. A search ranks its
/// hits, so a cap this size leaves nothing a reader would scroll to.
const FIND_ALL_LIMIT: usize = 1000;

pub async fn find(args: SessionFindArgs) -> Result<(), CliError> {
    let mut corpora: Vec<Corpus> = args.sources.iter().cloned().map(Corpus::Native).collect();
    if args.stored {
        corpora.push(Corpus::Stored);
    }
    if corpora.is_empty() {
        corpora.push(Corpus::Native(String::new()));
    }
    let store = match args.stored {
        true => Some(Arc::new(Mutex::new(
            Store::open(&gage_store::store_path()).context("gage session find")?,
        ))),
        false => None,
    };
    let ctx = ContextBuilder::new(store).build().await;
    let fetch = args.limit.fetch_limit().unwrap_or(FIND_ALL_LIMIT);
    let registry = source::driver_registry();

    // One query per corpus, run at once; each is ranked and capped on
    // its own, then the union is ranked and cut.
    let mut tasks = JoinSet::new();
    for (order, corpus) in corpora.iter().enumerate() {
        let ctx = ctx.clone();
        let sql = corpus.sql(&args.query, fetch);
        let corpus = corpus.clone();
        tasks.spawn(async move {
            let batches = run_query(&ctx, &sql).await;
            (order, corpus, batches)
        });
    }
    let mut hits: Vec<Hit> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let (order, corpus, batches) = joined.context("gage session find")?;
        let batches =
            batches.with_context(|| format!("gage session find: {}", corpus.describe()))?;
        hits.extend(hits_from_batches(&batches, order, &corpus, &registry)?);
    }
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.corpus_order.cmp(&b.corpus_order))
    });
    let total = hits.len();
    let show = args.limit.show_count(total);
    hits.truncate(show);
    if total == 0 {
        println!("No matches found");
        return Ok(());
    }
    attach_timestamps(&ctx, &corpora, &mut hits).await?;
    if args.by_date {
        sort_hits_by_date(&mut hits);
    }
    render_hits(&hits, corpora.len() > 1, args.full_id);
    args.limit.print_summary(show, total, "hit");
    Ok(())
}

/// One body of sessions a search covers
#[derive(Clone)]
enum Corpus {
    /// A session source, by its `--source` value; empty is the
    /// default source
    Native(String),
    Stored,
}

impl Corpus {
    /// The hits of this corpus for `query`, each joined to its
    /// session row for the id's display forms, best match first. The
    /// search itself is capped at `fetch` inside the function call,
    /// where the limit reaches the index.
    fn sql(&self, query: &str, fetch: usize) -> String {
        let query = query.replace('\'', "''");
        match self {
            Corpus::Native(spec) => {
                let spec = spec.replace('\'', "''");
                format!(
                    "SELECT t.session_id, s.id_display, s.id_prefix, t.line, t.type, t.snippet, \
                     t.score, '' AS driver \
                     FROM (SELECT * FROM native_message_text('{spec}', '{query}') LIMIT {fetch}) t \
                     JOIN native_session('{spec}') s ON s.id = t.session_id \
                     ORDER BY t.score DESC"
                )
            }
            Corpus::Stored => format!(
                "SELECT t.session_id, s.id_display, s.id_prefix, t.line, t.type, t.snippet, \
                 t.score, s.driver \
                 FROM (SELECT * FROM message_text('{query}') LIMIT {fetch}) t \
                 JOIN session s ON s.id = t.session_id \
                 ORDER BY t.score DESC"
            ),
        }
    }

    /// The timestamps of the messages behind `hits`, which all belong
    /// to this corpus. Each key is a literal list, so the scan opens
    /// only the hit sessions and masks to the hit lines; the cross
    /// product of the two lists is matched back to hits by
    /// `(session_id, line)`.
    fn timestamp_sql(&self, hits: &[&Hit]) -> String {
        let table = match self {
            Corpus::Native(spec) => format!("native_message('{}')", spec.replace('\'', "''")),
            Corpus::Stored => "message".to_string(),
        };
        let ids = unique_literals(
            hits.iter()
                .map(|h| format!("'{}'", h.id.replace('\'', "''"))),
        );
        let lines = unique_literals(hits.iter().map(|h| h.line.to_string()));
        format!(
            "SELECT session_id, line, timestamp FROM {table} \
             WHERE session_id IN ({ids}) AND line IN ({lines})"
        )
    }

    /// The corpus as the Source column names it
    fn label(&self) -> String {
        match self {
            Corpus::Native(spec) if spec.is_empty() => "default".to_string(),
            Corpus::Native(spec) => spec.clone(),
            Corpus::Stored => "stored".to_string(),
        }
    }

    /// The corpus as an error names it
    fn describe(&self) -> String {
        match self {
            Corpus::Native(spec) if spec.is_empty() => "searching default source".to_string(),
            Corpus::Native(spec) => format!("searching source {spec}"),
            Corpus::Stored => "searching stored sessions".to_string(),
        }
    }

    fn id_kind(&self) -> IdKind {
        match self {
            Corpus::Native(_) => IdKind::Native,
            Corpus::Stored => IdKind::Gage,
        }
    }
}

/// The distinct items of `literals`, comma-separated for an `IN` list
fn unique_literals(literals: impl Iterator<Item = String>) -> String {
    let unique: BTreeSet<String> = literals.collect();
    unique.into_iter().collect::<Vec<_>>().join(", ")
}

struct Hit {
    /// Position of the hit's corpus in the search order, the tiebreak
    /// between equal scores
    corpus_order: usize,
    corpus_label: String,
    id_kind: IdKind,
    id: String,
    id_display: String,
    id_prefix: String,
    line: i64,
    type_: String,
    snippet: String,
    score: f32,
    /// The message's timestamp, epoch milliseconds; `None` until
    /// looked up, or when the message has none
    timestamp_ms: Option<i64>,
}

fn hits_from_batches(
    batches: &[RecordBatch],
    corpus_order: usize,
    corpus: &Corpus,
    registry: &DriverRegistry,
) -> anyhow::Result<Vec<Hit>> {
    let mut out = Vec::new();
    for batch in batches {
        let ids = column::<StringArray>(batch, 0);
        let displays = column::<StringArray>(batch, 1);
        let prefixes = column::<StringArray>(batch, 2);
        let lines = column::<Int64Array>(batch, 3);
        let types = column::<StringArray>(batch, 4);
        let snippets = column::<StringArray>(batch, 5);
        let scores = column::<Float32Array>(batch, 6);
        let drivers = column::<StringArray>(batch, 7);
        for i in 0..batch.num_rows() {
            // A stored row names its driver; the id's display form is
            // the driver's, so an unknown driver is an error here as
            // it is in a listing.
            if let Corpus::Stored = corpus {
                let name = driver_name_of(drivers.value(i));
                registry
                    .for_name(name)
                    .ok_or_else(|| anyhow!("{}: unknown session driver '{name}'", ids.value(i)))?;
            }
            out.push(Hit {
                corpus_order,
                corpus_label: corpus.label(),
                id_kind: corpus.id_kind(),
                id: ids.value(i).to_string(),
                id_display: string_or_empty(displays, i),
                id_prefix: string_or_empty(prefixes, i),
                line: lines.value(i),
                type_: string_or_empty(types, i),
                snippet: snippets.value(i).to_string(),
                score: scores.value(i),
                timestamp_ms: None,
            });
        }
    }
    Ok(out)
}

/// Fill each hit's timestamp from its corpus's message table. The
/// search index carries no timestamp, so the shown hits are looked up
/// after the cut, one query per corpus with hits.
async fn attach_timestamps(
    ctx: &SessionContext,
    corpora: &[Corpus],
    hits: &mut [Hit],
) -> anyhow::Result<()> {
    for (order, corpus) in corpora.iter().enumerate() {
        let in_corpus: Vec<&Hit> = hits.iter().filter(|h| h.corpus_order == order).collect();
        if in_corpus.is_empty() {
            continue;
        }
        let sql = corpus.timestamp_sql(&in_corpus);
        let batches = run_query(ctx, &sql)
            .await
            .with_context(|| format!("gage session find: {}", corpus.describe()))?;
        let mut times: HashMap<(String, i64), i64> = HashMap::new();
        for batch in &batches {
            let ids = column::<StringArray>(batch, 0);
            let lines = column::<Int64Array>(batch, 1);
            let stamps = column::<TimestampMillisecondArray>(batch, 2);
            for i in 0..batch.num_rows() {
                if stamps.is_valid(i) {
                    times.insert((ids.value(i).to_string(), lines.value(i)), stamps.value(i));
                }
            }
        }
        for hit in hits.iter_mut().filter(|h| h.corpus_order == order) {
            hit.timestamp_ms = times.get(&(hit.id.clone(), hit.line)).copied();
        }
    }
    Ok(())
}

/// Newest first; a hit with no timestamp sorts last. Hits of one
/// time keep their score order.
fn sort_hits_by_date(hits: &mut [Hit]) {
    hits.sort_by(|a, b| match (a.timestamp_ms, b.timestamp_ms) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });
}

/// The hits as a table: the source when more than one was searched,
/// the message as `<session>:<line>`, the target `gage session view`
/// opens, the message type, the message's age, and the snippet with
/// its matches emphasized.
fn render_hits(hits: &[Hit], show_source: bool, full_id: bool) {
    let mut header = Vec::new();
    if show_source {
        header.push("Source");
    }
    header.extend(["Message", "Type", "Date", "Snippet"]);
    let message_col = usize::from(show_source);
    let type_col = message_col + 1;
    let date_col = type_col + 1;
    let snippet_col = header.len() - 1;
    let rows = hits.iter().map(|h| {
        let shown = if full_id { &h.id } else { &h.id_display };
        let mut cells = Vec::new();
        if show_source {
            cells.push(h.corpus_label.clone());
        }
        cells.extend([
            styled_target(shown, &h.id_prefix, h.id_kind, &format!(":{}", h.line)),
            h.type_.clone(),
            h.timestamp_ms.map(format_elapsed_ms).unwrap_or_default(),
            emphasize_matches(&h.snippet),
        ]);
        cells
    });
    let header: Vec<String> = header.into_iter().map(String::from).collect();
    let mut table = Table::from_iter(std::iter::once(header).chain(rows));
    table
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(
            Columns::new(type_col..date_col + 1).not(Rows::first()),
            style::dim(),
        );
    let term_width = console::Term::stdout().size().1 as usize;
    // The message cell is the target a reader acts on; it is never cut
    table.with(
        Width::truncate(term_width).suffix("…").priority(
            style::IdAwarePriority::new(true)
                .id_column(message_col)
                .weight(snippet_col, 3),
        ),
    );
    println!("{table}");
}

/// The snippet on one line, each guillemet-marked match shown in the
/// match color when the output is a terminal.
fn emphasize_matches(snippet: &str) -> String {
    let flat = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(flat.len());
    let mut rest = flat.as_str();
    while let Some(start) = rest.find('«') {
        out.push_str(&rest[..start]);
        let after = &rest[start + '«'.len_utf8()..];
        match after.find('»') {
            Some(end) => {
                out.push_str(&console::style(&after[..end]).yellow().bold().to_string());
                rest = &after[end + '»'.len_utf8()..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn remove(
    source: Option<String>,
    stored: bool,
    args: SessionRemoveArgs,
) -> Result<(), CliError> {
    if source.is_some() {
        return Err(CliError::message(
            "gage session remove: --source does not apply; sessions are removed from the store",
        ));
    }
    if stored {
        eprintln!("warning: --stored is redundant; sessions are removed from the store");
    }
    let store = Store::open(&gage_store::store_path()).context("gage session remove")?;
    let sessions = SessionStore::from(&store);

    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut records: Vec<SessionRecord> = Vec::with_capacity(args.ids.len());
    let mut errors = 0;
    for prefix in &args.ids {
        match sessions.get(prefix) {
            Ok(record) => {
                if !records.iter().any(|r| r.id == record.id) {
                    records.push(record);
                }
            }
            Err(e) => {
                eprintln!("gage session remove: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        return Err(CliError::exit(1));
    }
    let dataset_id = match args.dataset.as_deref() {
        Some(prefix) => Some(
            DatasetStore::from(&store)
                .resolve_id(prefix)
                .with_context(|| format!("gage session remove: --dataset {prefix}"))?,
        ),
        None => None,
    };

    dialog::run("Remove sessions", || {
        let lines: Vec<SessionLine> = records.iter().map(SessionLine::from).collect();
        preview_sessions(&lines, IdKind::Gage, None)?;
        if let Some(id) = &dataset_id {
            dialog::step_value("Dataset", short_uuid(id))?;
        }

        cli::log::remark(match &dataset_id {
            Some(_) => "Sessions stay in the store",
            None => "Native sessions are unaffected",
        })?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        // Every argument resolved above, so the single commit below
        // takes the whole set or nothing
        let ids: Vec<String> = records.iter().map(|r| r.id.clone()).collect();
        match &dataset_id {
            Some(dataset_id) => {
                let outcomes = DatasetStore::from(&store)
                    .sessions_unlink(dataset_id, &ids)
                    .map_err(|e| DialogError::Failed(e.to_string()))?;
                Ok(format!(
                    "Removed {} from dataset {}",
                    count(outcomes.len(), "session"),
                    short_uuid(dataset_id)
                )
                .into())
            }
            None => {
                let outcome = sessions
                    .remove(&ids)
                    .map_err(|e| DialogError::Failed(e.to_string()))?;
                if !outcome.datasets.is_empty() {
                    dialog::step_list(
                        "Datasets",
                        outcome.datasets.iter().map(|m| {
                            format!(
                                "{} ({} removed)",
                                short_uuid(&m.dataset_id),
                                count(m.session_ids.len(), "session")
                            )
                        }),
                    )?;
                }
                Ok(format!("Removed {}", count(outcome.sessions.len(), "session")).into())
            }
        }
    })
}

pub async fn delete(
    source: Option<String>,
    stored: bool,
    args: SessionDeleteArgs,
) -> Result<(), CliError> {
    if stored {
        return Err(CliError::message(
            "gage session delete: --stored does not apply; sessions are deleted from a source",
        ));
    }
    if args.ids.is_empty() && !args.empty {
        return Err(CliError::message(
            "gage session delete: provide session IDs or --empty\n\n\
            Use 'gage session list' to show sessions",
        ));
    }

    let registry = source::driver_registry();
    let (driver, spec) = source::resolve_source(&registry, source.as_deref())
        .map_err(|e| anyhow!("gage session delete: {e}"))?;
    let source = driver
        .open_source(&SourceOptions::new(&spec))
        .with_context(|| format!("gage session delete: {spec}"))?;
    let outcome = dialog::run_async("Delete sessions", || {
        delete_dialog(&args, source.as_ref(), &spec)
    })
    .await;
    source
        .close()
        .with_context(|| format!("gage session delete: {spec}"))?;
    outcome
}

async fn delete_dialog(
    args: &SessionDeleteArgs,
    source: &dyn Source,
    spec: &str,
) -> Result<DialogResult, DialogError> {
    // The empty-session and id-resolution queries run against the
    // native session rows through the `native_session` table function;
    // the source handle stays only for the driver deletes below.
    let ctx = ContextBuilder::new(None).build().await;
    let from = format!("native_session('{}')", spec.replace('\'', "''"));

    let targets = if args.empty {
        let spinner = cli::spinner();
        spinner.start("Looking for empty sessions");
        let targets = delete_targets(
            &ctx,
            &format!("SELECT id, title FROM {from} WHERE is_empty"),
        )
        .await
        .map_err(|e| DialogError::Other(e.into()))?;
        spinner.clear();
        targets
    } else {
        let ids = find_delete_ids(&args.ids, |prefix| source.find_native(prefix))?;
        let in_list = ids
            .iter()
            .map(|id| format!("'{}'", id.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT id, title FROM {from} WHERE id IN ({in_list})");
        delete_targets(&ctx, &sql)
            .await
            .map_err(|e| DialogError::Other(e.into()))?
    };
    if targets.is_empty() {
        return Ok("Nothing to delete".into());
    }

    let total = targets.len();
    preview_sessions(&targets, IdKind::Native, None)?;

    cli::log::remark("This cannot be undone")?;
    if !args.yes {
        let confirmed = cli::confirm("Continue?").initial_value(false).interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    let mut deleted = 0;
    for t in &targets {
        match source.delete_native(&t.id) {
            Ok(_) => deleted += 1,
            Err(e) => cli::log::warning(format!("{}: {e}", short_uuid(&t.id)))?,
        }
    }
    if deleted < total {
        return Err(dialog::partial("Deleted", deleted, total, "session"));
    }
    Ok(format!("Deleted {}", count(deleted, "session")).into())
}

pub fn move_(source: Option<String>, stored: bool, args: SessionMoveArgs) -> Result<(), CliError> {
    if stored {
        return Err(CliError::message(
            "gage session move: --stored does not apply; sessions are moved within a source",
        ));
    }
    let registry = source::driver_registry();
    let (driver, spec) = source::resolve_source(&registry, source.as_deref())
        .map_err(|e| anyhow!("gage session move: {e}"))?;
    let source = driver
        .open_source(&SourceOptions::new(&spec))
        .with_context(|| format!("gage session move: {spec}"))?;
    let outcome = dialog::run("Move session", || move_dialog(&args, source.as_ref()));
    source
        .close()
        .with_context(|| format!("gage session move: {spec}"))?;
    outcome
}

fn move_dialog(args: &SessionMoveArgs, source: &dyn Source) -> Result<DialogResult, DialogError> {
    let id = source
        .find_native(&args.session)
        .map_err(|e| DialogError::Failed(format!("{}: {e}", args.session)))?;
    let dir = fs::canonicalize(&args.dir)
        .map_err(|e| DialogError::Failed(format!("{}: {e}", args.dir.display())))?;
    if !args.force {
        match source.is_session_live(&id) {
            Ok(false) => {}
            Ok(true) => {
                return Err(DialogError::Failed(format!(
                    "Session {} is active. Close the session and run this command again.",
                    short_uuid(&id)
                )));
            }
            Err(SessionLiveError::Unknown(why)) => {
                cli::log::warning(format!("Cannot tell whether session is open: {why}"))?;
                return Err(DialogError::Failed(
                    "Confirm the session is not open and use --force to move it".to_string(),
                ));
            }
            Err(SessionLiveError::Driver(e)) => return Err(DialogError::Other(e.into())),
        }
    }

    cli::log::remark(format!("Session: {}", short_uuid(&id)))?;
    cli::log::remark(format!("To: {}", dir.display()))?;
    if !args.yes {
        let confirmed = cli::confirm("Continue?").initial_value(false).interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    source
        .move_native(&id, &dir)
        .map_err(|e| DialogError::Failed(e.to_string()))?;
    Ok(format!("Moved session {} to {}", short_uuid(&id), dir.display()).into())
}

/// Resolve every prefix before anything is deleted, so one bad
/// argument leaves the source untouched. The prefixes that match no
/// session fail the dialog as one list; any other lookup error is
/// reported on its own line first.
fn find_delete_ids(
    prefixes: &[String],
    find: impl Fn(&str) -> Result<String, NativeLookupError>,
) -> Result<Vec<String>, DialogError> {
    let mut ids: Vec<String> = Vec::with_capacity(prefixes.len());
    let mut missing: Vec<&str> = Vec::new();
    let mut errors = 0;
    for prefix in prefixes {
        match find(prefix) {
            Ok(id) => ids.push(id),
            Err(NativeLookupError::NoMatch(_)) => missing.push(prefix),
            Err(e) => {
                cli::log::error(format!("{prefix}: {e}"))?;
                errors += 1;
            }
        }
    }
    if !missing.is_empty() {
        return Err(DialogError::Failed(format!(
            "No sessions match: {}",
            missing.join(", ")
        )));
    }
    if errors > 0 {
        return Err(DialogError::Failed("Unknown sessions".to_string()));
    }
    Ok(ids)
}

/// Run `sql`, which selects `id` and `title`, as the delete targets
async fn delete_targets(
    ctx: &SessionContext,
    sql: &str,
) -> datafusion::error::Result<Vec<SessionLine>> {
    Ok(delete_lines(&run_query(ctx, sql).await?))
}

/// The sessions in `batches`, whose columns are `id` and `title`; a
/// null title is absent
fn delete_lines(batches: &[RecordBatch]) -> Vec<SessionLine> {
    let mut lines = Vec::new();
    for batch in batches {
        let ids = column::<StringArray>(batch, 0);
        let titles = column::<StringArray>(batch, 1);
        for i in 0..batch.num_rows() {
            lines.push(SessionLine {
                id: ids.value(i).to_string(),
                title: (!titles.is_null(i)).then(|| titles.value(i).to_string()),
            });
        }
    }
    lines
}

pub async fn view(
    source: Option<String>,
    stored: bool,
    args: SessionViewArgs,
) -> Result<(), CliError> {
    if !io::stdout().is_terminal() {
        return Err(CliError::message(
            "gage session view: standard output is not a terminal; read sessions with gage \
             query (tables session, message, tool_use)",
        ));
    }
    let options = ViewOptions::parse(args.options.as_deref().unwrap_or_default())
        .map_err(|e| anyhow!("gage session view: {e}"))?;
    let (prefix, line) = match args.session.as_deref() {
        Some(arg) => {
            let (prefix, line) =
                split_view_target(arg).map_err(|e| anyhow!("gage session view: {e}"))?;
            (Some(prefix), line)
        }
        None => (None, None),
    };
    // No session arg: the view opens with its session picker dialog.
    let result = if stored {
        view_stored(prefix, line, options).await
    } else {
        view_native(source.as_deref(), prefix, line, options).await
    };
    result.map_err(|e| anyhow!("gage session view: {e}").into())
}

/// Split a view target into its session prefix and the first line of
/// its `:LINES` suffix, when it has one.
fn split_view_target(arg: &str) -> Result<(&str, Option<u32>), Box<dyn Error>> {
    let Some((prefix, lines)) = arg.split_once(':') else {
        return Ok((arg, None));
    };
    let selection: LineSelection = lines.parse()?;
    let (first, _) = selection
        .ranges()
        .first()
        .copied()
        .expect("a parsed line selection should hold at least one range");
    let first = u32::try_from(first).map_err(|e| format!("line {first} is out of range: {e}"))?;
    Ok((prefix, Some(first)))
}

async fn view_stored(
    prefix: Option<&str>,
    line: Option<u32>,
    options: ViewOptions,
) -> Result<(), Box<dyn Error>> {
    let store = Store::open(&gage_store::store_path())?;
    let id = match prefix {
        Some(prefix) => Some(resolve_view_target(&store, prefix)?),
        None => None,
    };
    let backend = Backend::stored(store).await;
    session_view::run(backend, id.as_deref(), line, options).await
}

/// Resolve a view target to a session id. A stored session resolves
/// through the store; when `GAGE_SHOW_PUBLISHING` is set, a prefix
/// that matches no stored session is resolved against the pending
/// publish working trees, so a publish in progress can be viewed.
fn resolve_view_target(store: &Store, prefix: &str) -> Result<String, Box<dyn Error>> {
    match SessionStore::from(store).get(prefix) {
        Ok(record) => Ok(record.id),
        Err(stored_err) => {
            if show_publishing() {
                let ids: Vec<String> = PublishStore::from(store)
                    .list()?
                    .into_iter()
                    .map(|p| p.id)
                    .collect();
                if let Some(id) = pick_pending(prefix, &ids)? {
                    return Ok(id);
                }
            }
            Err(stored_err.into())
        }
    }
}

/// The one pending session id that starts with `prefix`: `None` when
/// none match, an error when more than one does.
fn pick_pending(prefix: &str, ids: &[String]) -> Result<Option<String>, String> {
    let mut matches = ids.iter().filter(|id| id.starts_with(prefix));
    let first = matches.next().cloned();
    if first.is_some() && matches.next().is_some() {
        return Err(format!(
            "ambiguous id prefix {prefix}: matches more than one pending publish session"
        ));
    }
    Ok(first)
}

async fn view_native(
    source: Option<&str>,
    prefix: Option<&str>,
    line: Option<u32>,
    options: ViewOptions,
) -> Result<(), Box<dyn Error>> {
    let registry = source::driver_registry();
    let (driver, spec) = source::resolve_source(&registry, source)?;
    let opened = driver.open_source(&SourceOptions::new(&spec))?;
    let id = match prefix {
        Some(prefix) => Some(opened.find_native(prefix)?),
        None => {
            warm_picker_sessions(opened.as_ref())?;
            None
        }
    };
    opened.close()?;
    let backend = Backend::native(&spec).open().await?;
    session_view::run(backend, id.as_deref(), line, options).await
}

/// Prepare the picker's sessions before the TUI takes the terminal,
/// behind a progress bar when any of them need reading.
fn warm_picker_sessions(source: &dyn Source) -> Result<(), Box<dyn Error>> {
    let mut bar: Option<cli::ProgressBar> = None;
    source.warm_summaries(picker_page_size(), &mut |done, total| {
        if total == 0 {
            return;
        }
        let bar = bar.get_or_insert_with(|| {
            let bar = cli::progress_bar(total as u64);
            bar.start("Reading sessions");
            bar
        });
        bar.set_position(done as u64);
    })?;
    if let Some(bar) = bar {
        bar.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prefixes that match no session fail as one list; a full set
    /// of matches resolves in argument order.
    #[test]
    fn find_delete_ids_lists_every_unmatched_prefix_in_one_failure() {
        let find = |prefix: &str| match prefix {
            "a" => Ok("a-full".to_string()),
            "b" => Ok("b-full".to_string()),
            other => Err(NativeLookupError::NoMatch(other.to_string())),
        };
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let Ok(ids) = find_delete_ids(&args(&["a", "b"]), find) else {
            panic!("matched prefixes should resolve");
        };
        assert_eq!(ids, ["a-full", "b-full"]);
        let Err(DialogError::Failed(msg)) = find_delete_ids(&args(&["832", "a", "579"]), find)
        else {
            panic!("unmatched prefixes should fail the dialog");
        };
        assert_eq!(msg, "No sessions match: 832, 579");
    }

    /// A delete target carries the row's id and title; a null title
    /// is absent.
    #[test]
    fn delete_lines_read_id_and_optional_title() {
        let batch = RecordBatch::try_from_iter(vec![
            (
                "id",
                Arc::new(StringArray::from(vec!["a1", "b2"])) as Arc<dyn Array>,
            ),
            (
                "title",
                Arc::new(StringArray::from(vec![Some("Fix the duck"), None])),
            ),
        ])
        .unwrap();
        let lines = delete_lines(&[batch]);
        assert_eq!(
            lines
                .iter()
                .map(|l| (l.id.as_str(), l.title.as_deref()))
                .collect::<Vec<_>>(),
            [("a1", Some("Fix the duck")), ("b2", None)]
        );
    }

    #[test]
    fn pick_pending_resolves_a_unique_prefix() {
        let ids = vec!["abc123".to_string(), "def456".to_string()];
        assert_eq!(
            pick_pending("abc", &ids).unwrap().as_deref(),
            Some("abc123")
        );
        assert_eq!(
            pick_pending("def456", &ids).unwrap().as_deref(),
            Some("def456")
        );
        assert_eq!(pick_pending("zzz", &ids).unwrap(), None);
    }

    #[test]
    fn pick_pending_rejects_an_ambiguous_prefix() {
        let ids = vec!["abc1".to_string(), "abc2".to_string()];
        assert!(pick_pending("abc", &ids).is_err());
    }

    fn hit(line: i64, score: f32, timestamp_ms: Option<i64>) -> Hit {
        Hit {
            corpus_order: 0,
            corpus_label: String::new(),
            id_kind: IdKind::Native,
            id: String::new(),
            id_display: String::new(),
            id_prefix: String::new(),
            line,
            type_: String::new(),
            snippet: String::new(),
            score,
            timestamp_ms,
        }
    }

    /// `--by-date` orders the hits newest first, keeps the score
    /// order among hits of one time, and puts hits without a
    /// timestamp last.
    #[test]
    fn by_date_sorts_newest_first_and_undated_last() {
        let mut hits = vec![
            hit(1, 0.9, None),
            hit(2, 0.8, Some(100)),
            hit(3, 0.7, Some(300)),
            hit(4, 0.6, Some(300)),
            hit(5, 0.5, Some(200)),
        ];
        sort_hits_by_date(&mut hits);
        let lines: Vec<i64> = hits.iter().map(|h| h.line).collect();
        assert_eq!(lines, [3, 4, 5, 2, 1]);
    }

    /// `--tag` is accepted with exactly one positional session and
    /// no filter flags.
    #[test]
    fn tag_requires_one_positional_session() {
        fn args(sessions: &[&str], filter: SessionFilterArgs) -> SessionAddArgs {
            SessionAddArgs {
                sessions: sessions.iter().map(|s| s.to_string()).collect(),
                dataset: None,
                filter,
                size: SizeLimitArgs { max_size: None },
                tag: Some("baseline".to_string()),
                force_tag: false,
                yes: true,
            }
        }
        let no_filter = || SessionFilterArgs {
            project: None,
            days: None,
            today: false,
            all: false,
            limit: None,
            sample: None,
        };
        assert!(selects_one_session(&args(&["7260"], no_filter())));
        assert!(!selects_one_session(&args(&[], no_filter())));
        assert!(!selects_one_session(&args(&["7260", "2956"], no_filter())));
        let mut filtered = no_filter();
        filtered.all = true;
        assert!(!selects_one_session(&args(&["7260"], filtered)));
    }

    #[test]
    fn view_target_splits_prefix_and_first_line() {
        assert_eq!(split_view_target("7260").unwrap(), ("7260", None));
        assert_eq!(
            split_view_target("7260:2956").unwrap(),
            ("7260", Some(2956))
        );
        assert_eq!(
            split_view_target("7260:10-20,30").unwrap(),
            ("7260", Some(10))
        );
        assert_eq!(
            split_view_target("7260:30,10-20").unwrap(),
            ("7260", Some(30))
        );
    }

    #[test]
    fn view_target_rejects_a_malformed_line_suffix() {
        for bad in ["7260:", "7260:abc", "7260:0", "7260:20-10"] {
            let err = split_view_target(bad).unwrap_err().to_string();
            assert!(err.contains("line selection"), "{bad}: {err}");
        }
        let beyond = u64::from(u32::MAX) + 1;
        let err = split_view_target(&format!("7260:{beyond}"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("out of range"), "{err}");
    }
}
