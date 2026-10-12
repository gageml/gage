use std::fmt;
use std::sync::{Arc, Mutex};

use clap::{Args, Subcommand};
use cliclack as cli;
use datafusion::arrow::array::{Int64Array, StringArray, TimestampMillisecondArray};
use gage_core::datetime::ms_to_iso8601;
use gage_core::uuid::short_uuid;
use gage_query::ContextBuilder;
use gage_store::{
    AddStep, DatasetSessionAddOutcome, DatasetStore, SessionOutcome, SessionSpec, Store,
    StoreError, TagStore,
};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::cmd_note::count_rows;
use crate::cmd_session::{column, run_query};
use crate::cmd_tag::TAG_HELP;
use crate::dialog::{self, DialogError};
use crate::human::format_elapsed_ms;
use crate::session_filter::{FILTER_ARG_NAMES, SessionFilterArgs, select_sessions};
use crate::size_limit::SizeLimitArgs;
use crate::source::SOURCE_HELP;
use crate::style::{self, IdKind, styled_id};

#[derive(Subcommand)]
pub enum DatasetCommand {
    /// Add a dataset
    ///
    /// Without session-selection options, adds an empty dataset. With
    /// options, adds a dataset populated with the selected native
    /// sessions in a single step.
    Add(DatasetAddArgs),

    /// Re-add each dataset session from its source
    ///
    /// Every member is re-read from the source and re-added: a
    /// session whose native content grew is updated in place.
    /// Equivalent to running gage session add -d DATASET on every
    /// member's native id.
    Refresh(DatasetRefreshArgs),

    /// List datasets
    List(DatasetListArgs),

    /// Show a dataset
    Show(DatasetShowArgs),

    /// Delete datasets
    ///
    /// Member sessions and scan runs that used a dataset are kept
    /// unless --sessions is given.
    Delete(DatasetDeleteArgs),
}

#[derive(Args)]
pub struct DatasetAddArgs {
    /// Session source
    #[arg(short, long, value_name = "SOURCE", long_help = format!("Session source\n\n{SOURCE_HELP}"))]
    pub source: Option<String>,

    /// Sessions to add (ID or prefix)
    #[arg(value_name = "SESSION", conflicts_with_all = FILTER_ARG_NAMES)]
    pub sessions: Vec<String>,

    #[command(flatten)]
    pub filter: SessionFilterArgs,

    #[command(flatten)]
    pub size: SizeLimitArgs,

    /// Tag new dataset
    #[arg(long, value_name = "NAME", long_help = format!("Tag new dataset\n\n{TAG_HELP}"))]
    pub tag: Option<String>,

    /// Move tag if it exists
    #[arg(long, requires = "tag")]
    pub force_tag: bool,
}

#[derive(Args)]
pub struct DatasetRefreshArgs {
    /// Dataset to refresh (ID or prefix)
    pub dataset: String,

    /// Session source
    #[arg(short, long, value_name = "SOURCE", long_help = format!("Session source\n\n{SOURCE_HELP}"))]
    pub source: Option<String>,

    #[command(flatten)]
    pub size: SizeLimitArgs,
}

#[derive(Args)]
pub struct DatasetListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,

    /// Show full dataset ID
    #[arg(long)]
    full_id: bool,
}

#[derive(Args)]
pub struct DatasetShowArgs {
    /// Dataset to show (ID or prefix)
    dataset: String,
}

#[derive(Args)]
pub struct DatasetDeleteArgs {
    /// Datasets to delete (ID or prefix)
    #[arg(required = true)]
    ids: Vec<String>,

    /// Also remove member sessions from store
    ///
    /// A session that another dataset holds is kept. Scan runs that
    /// used the dataset are not affected.
    #[arg(long)]
    sessions: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub async fn add(args: DatasetAddArgs) {
    let store = open_store("gage dataset add");
    let tags = TagStore::from(&store);
    // A tag that cannot be added is reported before the dataset
    // exists, so the failure leaves the store untouched
    if let Some(name) = &args.tag {
        match tags.check_available(name, args.force_tag) {
            Ok(()) => {}
            Err(e @ StoreError::TagExists { .. }) => {
                eprintln!("gage dataset add: {e} (use --force-tag to move it)");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("gage dataset add: {e}");
                std::process::exit(1);
            }
        }
    }
    let dataset_id = create_dataset("gage dataset add", &store);
    println!("Created dataset {}", short_uuid(&dataset_id));
    if let Some(name) = &args.tag {
        match tags.add(name, &dataset_id, args.force_tag) {
            Ok(added) => match added.previous_id {
                Some(previous) => println!(
                    "Moved tag {name} from {} to dataset {}",
                    short_uuid(&previous),
                    short_uuid(&dataset_id)
                ),
                None => println!("Tagged dataset {} as {name}", short_uuid(&dataset_id)),
            },
            Err(e) => {
                eprintln!("gage dataset add: {e}");
                std::process::exit(1);
            }
        }
    }

    if args.sessions.is_empty() && args.filter.is_empty() {
        return;
    }

    let max_bytes = Some(args.size.max_bytes());

    populate_dataset(
        "gage dataset add",
        &store,
        &dataset_id,
        args.source.as_deref(),
        &args.sessions,
        &args.filter,
        max_bytes,
    )
    .await;
}

pub async fn refresh(args: DatasetRefreshArgs) {
    let store = open_store("gage dataset refresh");
    let datasets = DatasetStore::from(&store);
    let record = match datasets.get(&args.dataset) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gage dataset refresh: {}: {e}", args.dataset);
            std::process::exit(1);
        }
    };
    let members = match datasets.sessions_list(&record.id) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("gage dataset refresh: {e}");
            std::process::exit(1);
        }
    };
    if members.is_empty() {
        println!(
            "Dataset {} has no sessions to refresh",
            short_uuid(&record.id)
        );
        return;
    }

    // Look each member's native id back up on disk. A member whose
    // native session is gone is a hard error; the dataset stays
    // untouched in that case.
    let mut selected = Vec::with_capacity(members.len());
    let mut errors = Vec::new();
    for member in &members {
        match gage_claude::session::one_session(&member.native_id) {
            Ok(info) => selected.push(info),
            Err(e) => errors.push(e.to_string()),
        }
    }
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("gage dataset refresh: {e}");
        }
        std::process::exit(1);
    }

    let max_bytes = Some(args.size.max_bytes());

    if let Err(e) = add_native_to_dataset(
        &store,
        &record.id,
        args.source.as_deref(),
        &selected,
        max_bytes,
        &mut print_added,
    ) {
        eprintln!("gage dataset refresh: {e}");
        std::process::exit(1);
    }
}

/// Create an empty dataset in `store`. Prints `command: <error>`
/// and exits on failure.
pub(crate) fn create_dataset(command: &str, store: &Store) -> String {
    match DatasetStore::from(store).create() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    }
}

/// Resolve `select` to native sessions and add them to
/// `dataset_id` through the opened source. Prints one
/// `Added session … (native …) to dataset …` line per added
/// session. On any failure it prints `command: <error>` and exits.
/// A selection that resolves to no sessions is announced and
/// otherwise is not an error.
pub(crate) async fn populate_dataset(
    command: &str,
    store: &Store,
    dataset_id: &str,
    source: Option<&str>,
    sessions: &[String],
    filter: &SessionFilterArgs,
    max_bytes: Option<u64>,
) {
    let selected = match select_sessions(sessions, filter).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    };
    if selected.is_empty() {
        println!("No sessions matched the selection; dataset is empty");
        return;
    }
    if let Err(e) = add_native_to_dataset(
        store,
        dataset_id,
        source,
        &selected,
        max_bytes,
        &mut print_added,
    ) {
        eprintln!("{command}: {e}");
        std::process::exit(1);
    }
}

/// Progress of a session add, reported to the caller as it happens.
/// Each event renders as the line a plain listing prints for it.
#[derive(Debug)]
pub(crate) enum AddEvent<'a> {
    /// A session, one of `total`, is about to be written
    Starting { total: usize, native_id: &'a str },
    /// A session was written and placed
    Added {
        outcome: SessionOutcome,
        id: &'a str,
        native_id: &'a str,
        dataset: Option<&'a str>,
    },
    /// Every session is placed. `placed` counts them all, whether
    /// written anew, updated, or unchanged.
    Done {
        placed: usize,
        dataset: Option<&'a str>,
    },
}

impl fmt::Display for AddEvent<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddEvent::Starting { native_id, .. } => {
                write!(f, "Adding session {}", short_uuid(native_id))
            }
            AddEvent::Added {
                outcome,
                id,
                native_id,
                dataset,
            } => {
                let verb = match outcome {
                    SessionOutcome::Added => "Added",
                    SessionOutcome::Updated => "Updated",
                    SessionOutcome::Unchanged => "Unchanged",
                };
                write!(
                    f,
                    "{verb} session {} (native {})",
                    short_uuid(id),
                    short_uuid(native_id)
                )?;
                match dataset {
                    Some(dataset) => write!(f, " to dataset {}", short_uuid(dataset)),
                    None => Ok(()),
                }
            }
            AddEvent::Done { placed, dataset } => {
                let noun = if *placed == 1 { "session" } else { "sessions" };
                write!(f, "Added {placed} {noun}")?;
                match dataset {
                    Some(dataset) => write!(f, " to dataset {}", short_uuid(dataset)),
                    None => Ok(()),
                }
            }
        }
    }
}

/// Add each already-resolved native session record to `dataset_id`
/// through the opened source in a single commit, reporting each
/// session to `on_event` as it is written. Performs no output of its
/// own. `sessions_add` is idempotent per session — a member whose
/// native content is unchanged is a no-op, a member whose content
/// grew is updated in its slot, and a non-member is appended.
pub(crate) fn add_native_to_dataset(
    store: &Store,
    dataset_id: &str,
    source: Option<&str>,
    selected: &[gage_claude::session::SessionInfo],
    max_bytes: Option<u64>,
    on_event: &mut dyn FnMut(AddEvent<'_>),
) -> Result<Vec<DatasetSessionAddOutcome>, String> {
    let registry = crate::source::driver_registry();
    let (driver, spec) = crate::source::resolve_source(&registry, source)?;
    let source_handle = driver
        .open_source(&spec)
        .map_err(|e| format!("{spec}: {e}"))?;

    // Confirm every selected session is present in the opened
    // source before writing anything, so one missing session leaves
    // the dataset unchanged rather than partially updated.
    let mut ids: Vec<String> = Vec::with_capacity(selected.len());
    let mut errors = Vec::new();
    for info in selected {
        match source_handle.find_native(&info.id) {
            Ok(id) => ids.push(id),
            Err(e) => errors.push(e.to_string()),
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }

    let mut natives = Vec::with_capacity(ids.len());
    for id in &ids {
        let native = source_handle
            .open_native(id)
            .map_err(|e| format!("{id}: {e}"))?;
        natives.push(native);
    }

    let specs: Vec<SessionSpec<'_>> = natives
        .iter_mut()
        .map(|session| SessionSpec {
            driver: driver.as_ref(),
            session: session.as_mut(),
        })
        .collect();
    let datasets = match max_bytes {
        Some(max) => DatasetStore::from(store).with_max_session_size(max),
        None => DatasetStore::from(store),
    };
    let total = ids.len();
    // The specs are written in `ids` order; the index of the session
    // being written names its native id for the outcome event
    let mut current = 0;
    let outcomes = datasets
        .sessions_add_with_progress(dataset_id, specs, &mut |step| match step {
            AddStep::Starting { index, native_id } => {
                current = index;
                on_event(AddEvent::Starting { total, native_id });
            }
            AddStep::Added(outcome) => on_event(AddEvent::Added {
                outcome: outcome.outcome,
                id: &outcome.id,
                native_id: ids.get(current).map_or("", String::as_str),
                dataset: Some(dataset_id),
            }),
        })
        .map_err(|e| e.to_string())?;
    on_event(AddEvent::Done {
        placed: outcomes.len(),
        dataset: Some(dataset_id),
    });

    source_handle.close().map_err(|e| format!("{spec}: {e}"))?;
    Ok(outcomes)
}

/// Print the outcome line of each session an add writes; the plain
/// listing the dataset commands show.
fn print_added(event: AddEvent<'_>) {
    if let AddEvent::Added { .. } = event {
        println!("{event}");
    }
}

pub async fn list(args: DatasetListArgs) {
    let store = open_store("gage dataset list");
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let total = count_rows(&ctx, "SELECT COUNT(*) FROM dataset").await;
    if total == 0 {
        println!("No datasets found");
        return;
    }
    let show = args.limit.show_count(total);
    let sql = format!(
        "SELECT d.id, d.id_prefix, d.created, \
         COALESCE(a.n, 0) AS attachments, COALESCE(m.n, 0) AS sessions, \
         COALESCE(sc.n, 0) AS scans, \
         arrow_cast(COALESCE(t.names, ''), 'Utf8') AS tags \
         FROM dataset d \
         LEFT JOIN (SELECT dataset_id, COUNT(*) AS n FROM dataset_attachment \
                    GROUP BY dataset_id) a ON a.dataset_id = d.id \
         LEFT JOIN (SELECT dataset_id, COUNT(*) AS n FROM dataset_session \
                    GROUP BY dataset_id) m ON m.dataset_id = d.id \
         LEFT JOIN (SELECT dataset, COUNT(*) AS n FROM scan \
                    WHERE dataset IS NOT NULL GROUP BY dataset) sc ON sc.dataset = d.id \
         LEFT JOIN (SELECT id, string_agg(name, ', ' ORDER BY name) AS names FROM tag \
                    GROUP BY id) t ON t.id = d.id \
         ORDER BY d.modified DESC LIMIT {show}"
    );
    let batches = run_query(&ctx, &sql).await;

    let header: Vec<String> = ["Id", "Sessions", "Attachments", "Scans", "Tags", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in &batches {
        let ids = column::<StringArray>(batch, 0);
        let prefixes = column::<StringArray>(batch, 1);
        let createds = column::<TimestampMillisecondArray>(batch, 2);
        let attachments = column::<Int64Array>(batch, 3);
        let sessions = column::<Int64Array>(batch, 4);
        let scans = column::<Int64Array>(batch, 5);
        let tags = column::<StringArray>(batch, 6);
        for i in 0..batch.num_rows() {
            let id = ids.value(i);
            let shown = if args.full_id { id } else { short_uuid(id) };
            rows.push(vec![
                styled_id(shown, prefixes.value(i), IdKind::Gage),
                sessions.value(i).to_string(),
                attachments.value(i).to_string(),
                scans.value(i).to_string(),
                tags.value(i).to_string(),
                format_elapsed_ms(createds.value(i)),
            ]);
        }
    }
    let shown = rows.len();
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::new(1..4), Alignment::right())
        .modify(
            Columns::one(4).not(Rows::first()),
            style::tty(Color::FG_BRIGHT_CYAN),
        )
        .modify(Columns::new(1..4).not(Rows::first()), style::dim())
        .modify(Columns::one(5).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    args.limit.print_summary(shown, total, "dataset");
}

pub async fn show(args: DatasetShowArgs) {
    let store = Arc::new(Mutex::new(open_store("gage dataset show")));
    fn or_exit<T>(result: Result<T, StoreError>) -> T {
        result.unwrap_or_else(|e| {
            eprintln!("gage dataset show: {e}");
            std::process::exit(1)
        })
    }
    let (record, sessions, attachments, tags, header) = {
        let store = store.lock().unwrap();
        let datasets = DatasetStore::from(&*store);
        let record = match datasets.get(&args.dataset) {
            Ok(record) => record,
            Err(e) => {
                eprintln!("gage dataset show: {}: {e}", args.dataset);
                std::process::exit(1);
            }
        };
        let sessions = or_exit(datasets.sessions(&record.id));
        let attachments = or_exit(datasets.attachments(&record.id));
        let tags = or_exit(TagStore::from(&*store).targets());
        let header = or_exit(store.read_header(&record.commit_sha));
        (record, sessions, attachments, tags, header)
    };
    let ctx = ContextBuilder::new(Some(store)).build().await;
    let sql = format!(
        "SELECT id FROM scan WHERE dataset = '{}' ORDER BY modified DESC",
        record.id
    );
    let scans: Vec<String> = run_query(&ctx, &sql)
        .await
        .iter()
        .flat_map(|batch| {
            let ids = column::<StringArray>(batch, 0);
            (0..batch.num_rows())
                .map(|i| ids.value(i).to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    let iso = |ms: Option<i64>| ms.map(ms_to_iso8601).unwrap_or_default();

    let sessions_cell = sessions
        .iter()
        .map(|s| s.id.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let attachments_cell = attachments
        .iter()
        .map(|a| format!("{} {}", a.id, a.attrs.name.as_deref().unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("\n");
    let scans_cell = scans.join("\n");
    let tags_cell = tags
        .iter()
        .filter(|t| t.id == record.id)
        .map(|t| t.name.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let rows = [
        ("id", record.id.clone()),
        ("sessions", sessions_cell),
        ("attachments", attachments_cell),
        ("scans", scans_cell),
        ("tags", tags_cell),
        ("created", iso(Some(record.created_ms))),
        ("modified", iso(header.modified_ms)),
    ];
    let table = Table::from_iter(rows.iter().map(|(k, v)| [k.to_string(), v.clone()]))
        .with(Style::rounded())
        .modify(Columns::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .to_string();
    println!("{table}");
}

pub fn delete(args: DatasetDeleteArgs) {
    let store = open_store("gage dataset delete");
    let datasets = DatasetStore::from(&store);

    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut records = Vec::with_capacity(args.ids.len());
    let mut errors = 0;
    for prefix in &args.ids {
        match datasets.get(prefix) {
            Ok(record) => records.push(record),
            Err(e) => {
                eprintln!("gage dataset delete: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let count = records.len();
    let members: usize = records.iter().map(|r| r.session_count).sum();
    dialog::run("Delete datasets", || {
        let mut remark = format!("{count} {}", plural(count, "dataset"));
        if args.sessions {
            remark.push_str(&format!(", {members} {}", plural(members, "session")));
        }
        cli::log::remark(remark)?;

        if !args.yes {
            let action = if args.sessions {
                format!(
                    "Permanently delete {count} {} and remove their sessions?",
                    plural(count, "dataset")
                )
            } else {
                format!("Permanently delete {count} {}?", plural(count, "dataset"))
            };
            let confirmed = cli::confirm(format!("{action} This cannot be undone."))
                .initial_value(false)
                .interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let mut deleted = 0;
        let mut removed = 0;
        let mut kept = 0;
        for record in &records {
            let result = if args.sessions {
                datasets.delete_cascade(&record.id).map(|d| {
                    removed += d.removed.len();
                    kept += d.kept.len();
                })
            } else {
                datasets.delete(&record.id).map(|_| ())
            };
            match result {
                Ok(()) => deleted += 1,
                Err(e) => eprintln!("warning: failed to delete {}: {e}", short_uuid(&record.id)),
            }
        }

        let mut outro = format!("Deleted {deleted} {}", plural(deleted, "dataset"));
        if args.sessions {
            outro.push_str(&format!(
                ", removed {removed} {}",
                plural(removed, "session")
            ));
            if kept > 0 {
                outro.push_str(&format!(", kept {kept} held by other datasets"));
            }
        }
        Ok(outro.into())
    });
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        noun.to_string()
    } else {
        format!("{noun}s")
    }
}

/// Open the default store, or print `command: <error>` and exit
fn open_store(command: &str) -> Store {
    match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    }
}
