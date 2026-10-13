use std::error::Error;
use std::io::{self, IsTerminal};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{ArgGroup, Args, Subcommand};
use cliclack as cli;
use datafusion::arrow::array::{
    Array, BooleanArray, Int64Array, StringArray, TimestampMillisecondArray,
};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use gage_core::uuid::short_uuid;
use gage_query::ContextBuilder;
use gage_registry::driver::DriverRegistry;
use gage_session::{Driver, Source};
use gage_store::{DatasetStore, SessionStore, Store};
use gage_tui::session::{Backend, picker_page_size};
use gage_tui::{ViewOptions, session_view};
use indicatif::ProgressBar;
use tabled::{
    Table,
    grid::{config::Position, records::RecordsMut},
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
    },
};

use crate::cmd_dataset::{AddEvent, add_native_to_dataset};
use crate::dialog::{self, DialogError};
use crate::session_filter::{FILTER_ARG_NAMES, SessionFilterArgs, select_sessions};
use crate::size_limit::SizeLimitArgs;
use crate::source;
use crate::style::{self, IdKind, styled_id};

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

    /// View a session
    View(SessionViewArgs),
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
}

#[derive(Args)]
#[command(next_display_order = 3)]
pub struct SessionViewArgs {
    /// Session to view (ID or prefix)
    pub session: Option<String>,

    /// Comma separated list of view options
    ///
    /// Options:
    ///   turns   show model turns in outline
    ///   detail  show all entries, not only high-signal ones
    #[arg(short = 'v', long, value_delimiter = ',', verbatim_doc_comment)]
    pub options: Vec<String>,
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

pub async fn list(source: Option<String>, stored: bool, args: SessionListArgs) {
    if args.dataset.is_some() {
        if source.is_some() {
            eprintln!(
                "gage session list: --source does not apply with --dataset; a dataset holds stored sessions"
            );
            std::process::exit(1);
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

async fn list_native(source: Option<String>, args: SessionListArgs) {
    let registry = source::driver_registry();
    let (driver, spec) = match source::resolve_source(&registry, source.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("gage session list: {e}");
            std::process::exit(1);
        }
    };
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
        query_sessions(&ctx, &args, Listing::Native, &from, project_pred.as_deref()).await;
    if total > 0 {
        let drivers: Vec<Arc<dyn Driver>> = vec![driver.clone(); rows.len()];
        render_table(&rows, &drivers, Listing::Native, args.full_id);
        args.limit.print_summary(rows.len(), total, "session");
    } else {
        println!("No sessions found");
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
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage session list: {e}");
            std::process::exit(1);
        }
    };
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let (rows, total) = query_sessions(&ctx, &args, Listing::Stored, "session", None).await;
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

/// List the member sessions of a dataset, each at the commit the
/// dataset links, in member order: the `session` table scoped to the
/// dataset, joined to `dataset_session` for the order.
async fn list_dataset(args: SessionListArgs) {
    if args.project.is_some() {
        eprintln!("gage session list: --project does not apply to stored sessions");
        std::process::exit(1);
    }
    let prefix = args
        .dataset
        .as_deref()
        .expect("list_dataset is called with --dataset");
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage session list: {e}");
            std::process::exit(1);
        }
    };
    let dataset = match DatasetStore::from(&store).get(prefix) {
        Ok(record) => record,
        Err(e) => {
            eprintln!("gage session list: --dataset {prefix}: {e}");
            std::process::exit(1);
        }
    };
    let store = Arc::new(Mutex::new(store));
    let ctx = ContextBuilder::new(Some(store)).build().await;
    let from = "session JOIN dataset_session m ON m.session_id = session.id";
    let member = format!("m.dataset_id = '{}'", dataset.id);
    let (rows, total) = query_sessions(&ctx, &args, Listing::Dataset, from, Some(&member)).await;
    if total == 0 {
        println!("No sessions found");
        return;
    }
    let registry = source::driver_registry();
    let drivers = match stored_drivers(&registry, &rows) {
        Ok(drivers) => drivers,
        Err(e) => {
            eprintln!("gage session list: {e}");
            std::process::exit(1);
        }
    };
    render_table(&rows, &drivers, Listing::Dataset, args.full_id);
    args.limit.print_summary(rows.len(), total, "session");
}

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
/// native binding has no type and fills it with an empty literal; its
/// time column is `mtime` where the store's is `created`, its size is
/// `size` where the store's is `native_size`, and rows order and
/// `--since` filter on `mtime` where the store's do on `modified`.
async fn query_sessions(
    ctx: &SessionContext,
    args: &SessionListArgs,
    listing: Listing,
    from: &str,
    project_pred: Option<&str>,
) -> (Vec<Row>, usize) {
    let where_clause = build_where_clause(args, listing, project_pred);
    let limit_clause = match args.limit.fetch_limit() {
        Some(n) => format!(" LIMIT {n}"),
        None => String::new(),
    };
    let (type_col, size_col, time_col, driver_col) = match listing {
        Listing::Native => (
            "'' AS session_type",
            "size",
            "mtime AS time",
            "'' AS driver",
        ),
        Listing::Stored | Listing::Dataset => (
            "session_type",
            "native_size AS size",
            "created AS time",
            "driver",
        ),
    };
    let order_clause = listing.order_clause();
    let sql = format!(
        "SELECT id, id_display, id_prefix, project, title, {type_col}, model, {size_col}, \
         message_count, {time_col}, {driver_col} \
         FROM {from}{where_clause} \
         ORDER BY {order_clause}{limit_clause}",
    );
    let batches = run_query(ctx, &sql).await;
    let rows = rows_from_batches(&batches);

    let count_sql = format!("SELECT COUNT(*) FROM {from}{where_clause}");
    let count_batches = run_query(ctx, &count_sql).await;
    let total = count_batches
        .first()
        .and_then(|b| b.column(0).as_any().downcast_ref::<Int64Array>())
        .map(|a| a.value(0) as usize)
        .unwrap_or(0);
    (rows, total)
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

/// Run `sql` and collect its batches, exiting with the error on
/// failure
pub(crate) async fn run_query(ctx: &SessionContext, sql: &str) -> Vec<RecordBatch> {
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

pub async fn add(source: Option<String>, stored: bool, args: SessionAddArgs) {
    if stored && args.dataset.is_none() {
        eprintln!(
            "gage session add: --stored applies only with --dataset; sessions are added from a source"
        );
        std::process::exit(1);
    }
    // --stored --dataset resolves stored session IDs, not the native
    // filter axes. Reject anything but positional session IDs here so
    // a filter given with --stored fails up front.
    if stored && (args.sessions.is_empty() || !args.filter.is_empty()) {
        eprintln!(
            "gage session add: --stored --dataset takes stored session IDs; the selection flags apply to native sessions"
        );
        std::process::exit(1);
    }
    dialog::run_async("Add sessions", || add_dialog(source, stored, args)).await;
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
    let max_bytes = Some(args.size.max_bytes());
    let mut view = DialogAddView::new(io::stdout().is_terminal());
    match dataset_id {
        Some(dataset_id) if stored => {
            add_stored_to_dataset(&store, &dataset_id, &args.sessions, &mut |e| view.on(e))?
        }
        _ => {
            let selected = select_sessions(source.as_deref(), &args.sessions, &args.filter)
                .await
                .map_err(|e| DialogError::Failed(format!("{e}")))?;
            if selected.is_empty() {
                return Err(DialogError::Failed(
                    "No sessions matched the selection".to_string(),
                ));
            }
            add_native(
                &store,
                source,
                dataset_id.as_deref(),
                &selected,
                max_bytes,
                &mut |e| view.on(e),
            )?
        }
    }
    Ok(view.finish().into())
}

/// Renders an add inside the `Add sessions` dialog: a progress bar
/// on a terminal, one outcome line per session otherwise, and the
/// closing line held for the outro.
struct DialogAddView {
    progress: bool,
    bar: Option<cli::ProgressBar>,
    done: String,
}

impl DialogAddView {
    fn new(progress: bool) -> Self {
        Self {
            progress,
            bar: None,
            done: String::new(),
        }
    }

    fn on(&mut self, event: AddEvent<'_>) {
        match &event {
            AddEvent::Starting { total, .. } => {
                if self.progress {
                    let bar = self.bar.get_or_insert_with(|| {
                        let bar = cli::progress_bar(*total as u64);
                        bar.start("Adding sessions");
                        bar
                    });
                    bar.set_message(event.to_string());
                }
            }
            AddEvent::Added { .. } => match &self.bar {
                Some(bar) => bar.inc(1),
                None => println!("{event}"),
            },
            AddEvent::Done { .. } => {
                if let Some(bar) = self.bar.take() {
                    bar.clear();
                }
                self.done = event.to_string();
            }
        }
    }

    /// The closing line, for the outro
    fn finish(self) -> String {
        self.done
    }
}

/// Link sessions already in the store, given by Gage id or prefix, as
/// members of `dataset_id`, reporting each to `on_event`.
fn add_stored_to_dataset(
    store: &Store,
    dataset_id: &str,
    prefixes: &[String],
    on_event: &mut dyn FnMut(AddEvent<'_>),
) -> Result<(), DialogError> {
    let sessions = SessionStore::from(store);

    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut records = Vec::with_capacity(prefixes.len());
    let mut errors = Vec::new();
    for prefix in prefixes {
        match sessions.get(prefix) {
            Ok(record) => records.push(record),
            Err(e) => errors.push(e.to_string()),
        }
    }
    if !errors.is_empty() {
        return Err(DialogError::Failed(errors.join("\n")));
    }

    let ids: Vec<String> = records.iter().map(|r| r.id.clone()).collect();
    let outcomes = DatasetStore::from(store)
        .sessions_link(dataset_id, &ids)
        .map_err(|e| DialogError::Failed(format!("{e}")))?;
    for (outcome, record) in outcomes.iter().zip(&records) {
        on_event(AddEvent::Added {
            outcome: outcome.outcome,
            id: &outcome.id,
            native_id: &record.attrs.native_id,
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
/// opened source before anything is written.
fn add_native(
    store: &Store,
    source: Option<String>,
    dataset_id: Option<&str>,
    selected: &[String],
    max_bytes: Option<u64>,
    on_event: &mut dyn FnMut(AddEvent<'_>),
) -> Result<(), DialogError> {
    if let Some(dataset_id) = dataset_id {
        add_native_to_dataset(
            store,
            dataset_id,
            source.as_deref(),
            selected,
            max_bytes,
            on_event,
        )
        .map_err(DialogError::Failed)?;
        return Ok(());
    }
    let registry = source::driver_registry();
    let (driver, spec) =
        source::resolve_source(&registry, source.as_deref()).map_err(DialogError::Failed)?;
    let opened = driver
        .open_source(&spec)
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

    let mut natives = Vec::with_capacity(ids.len());
    for id in &ids {
        let native = opened
            .open_native(id)
            .map_err(|e| DialogError::Failed(format!("{id}: {e}")))?;
        natives.push(native);
    }

    let sessions = match max_bytes {
        Some(max) => SessionStore::from(store).with_max_session_size(max),
        None => SessionStore::from(store),
    };
    let total = ids.len();
    for (session, id) in natives.iter_mut().zip(&ids) {
        on_event(AddEvent::Starting {
            total,
            native_id: id,
        });
        let outcome = sessions
            .add(driver.as_ref(), session.as_mut())
            .map_err(|e| DialogError::Failed(format!("{id}: {e}")))?;
        on_event(AddEvent::Added {
            outcome: outcome.outcome,
            id: &outcome.id,
            native_id: id,
            dataset: None,
        });
    }
    on_event(AddEvent::Done {
        placed: total,
        dataset: None,
    });
    opened
        .close()
        .map_err(|e| DialogError::Failed(format!("{spec}: {e}")))?;
    Ok(())
}

pub fn remove(source: Option<String>, stored: bool, args: SessionRemoveArgs) {
    if source.is_some() {
        eprintln!(
            "gage session remove: --source does not apply; sessions are removed from the store"
        );
        std::process::exit(1);
    }
    if stored {
        eprintln!("warning: --stored is redundant; sessions are removed from the store");
    }
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage session remove: {e}");
            std::process::exit(1);
        }
    };
    match args.dataset.as_deref() {
        Some(prefix) => remove_from_dataset(&store, prefix, &args.ids),
        None => remove_from_store(&store, &args.ids),
    }
}

/// Unlink sessions from one dataset; the session objects stay
fn remove_from_dataset(store: &Store, dataset_prefix: &str, ids: &[String]) {
    let datasets = DatasetStore::from(store);
    let dataset_id = match datasets.resolve_id(dataset_prefix) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage session remove: --dataset {dataset_prefix}: {e}");
            std::process::exit(1);
        }
    };
    // Every argument is resolved before the single dataset commit, so
    // one bad argument leaves the dataset untouched
    let outcomes = match datasets.sessions_unlink(&dataset_id, ids) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gage session remove: {e}");
            std::process::exit(1);
        }
    };
    for o in &outcomes {
        println!(
            "Removed session {} (native {}) from dataset {}",
            short_uuid(&o.id),
            short_uuid(&o.native_id),
            short_uuid(&dataset_id)
        );
    }
}

/// Unlink sessions from every dataset holding them and tombstone them
fn remove_from_store(store: &Store, ids: &[String]) {
    let sessions = SessionStore::from(store);

    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut errors = 0;
    for prefix in ids {
        if let Err(e) = sessions.get(prefix) {
            eprintln!("gage session remove: {e}");
            errors += 1;
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let outcome = match sessions.remove(ids) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gage session remove: {e}");
            std::process::exit(1);
        }
    };
    for members in &outcome.datasets {
        for id in &members.session_ids {
            let native_id = outcome
                .sessions
                .iter()
                .find(|r| &r.id == id)
                .map(|r| r.attrs.native_id.as_str())
                .unwrap_or_default();
            println!(
                "Removed session {} (native {}) from dataset {}",
                short_uuid(id),
                short_uuid(native_id),
                short_uuid(&members.dataset_id)
            );
        }
    }
    for record in &outcome.sessions {
        println!(
            "Removed session {} (native {})",
            short_uuid(&record.id),
            short_uuid(&record.attrs.native_id)
        );
    }
}

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
    // The empty-session and id-resolution queries run against the
    // native session rows through the `native_session` table function;
    // the source handle stays only for the driver deletes below.
    let ctx = ContextBuilder::new(None).build().await;
    let from = format!("native_session('{}')", spec.replace('\'', "''"));

    let targets = if args.empty {
        let spinner = style::spinner("Looking for empty sessions...");
        let targets = delete_targets(
            &ctx,
            &format!("SELECT id, is_empty FROM {from} WHERE is_empty"),
        )
        .await;
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
        let sql = format!("SELECT id, is_empty FROM {from} WHERE id IN ({in_list})");
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

pub async fn view(source: Option<String>, stored: bool, args: SessionViewArgs) {
    let options = match ViewOptions::parse(&args.options) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gage session view: {e}");
            std::process::exit(1);
        }
    };
    // No session arg: the view opens with its session picker dialog.
    let result = if stored {
        view_stored(args.session.as_deref(), options).await
    } else {
        view_native(source.as_deref(), args.session.as_deref(), options).await
    };
    if let Err(e) = result {
        eprintln!("gage session view: {e}");
        std::process::exit(1);
    }
}

async fn view_stored(prefix: Option<&str>, options: ViewOptions) -> Result<(), Box<dyn Error>> {
    let store = Store::open(&gage_store::store_path())?;
    let id = match prefix {
        Some(prefix) => Some(SessionStore::from(&store).get(prefix)?.id),
        None => None,
    };
    let backend = Backend::stored(store).await;
    session_view::run(backend, id.as_deref(), options).await
}

async fn view_native(
    source: Option<&str>,
    prefix: Option<&str>,
    options: ViewOptions,
) -> Result<(), Box<dyn Error>> {
    let registry = source::driver_registry();
    let (driver, spec) = source::resolve_source(&registry, source)?;
    let opened = driver.open_source(&spec)?;
    let id = match prefix {
        Some(prefix) => Some(opened.find_native(prefix)?),
        None => {
            warm_picker_sessions(opened.as_ref())?;
            None
        }
    };
    opened.close()?;
    let backend = Backend::native(&spec).await?;
    session_view::run(backend, id.as_deref(), options).await
}

/// Prepare the picker's sessions before the TUI takes the terminal,
/// behind a progress bar when any of them need reading.
fn warm_picker_sessions(source: &dyn Source) -> Result<(), Box<dyn Error>> {
    let mut bar: Option<ProgressBar> = None;
    source.warm_summaries(picker_page_size(), &mut |done, total| {
        if total == 0 {
            return;
        }
        let bar = bar.get_or_insert_with(|| {
            let bar = style::progress_bar_plain(total as u64);
            bar.set_message("Reading sessions");
            bar
        });
        bar.set_position(done as u64);
    })?;
    if let Some(bar) = bar {
        bar.finish_and_clear();
    }
    Ok(())
}
